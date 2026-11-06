//! T29-C03 b (产物来源与路径安全 — credential), native Local Repository mount.
//!
//! O3: logs and origin carry no credential. A real credentialed clone runs
//! against a local Gateway endpoint served by `git http-backend`; the server
//! demands HTTP Basic auth, so the capability is provably used on the wire.
//!
//! | Row | Clone | Effect |
//! |---|---|---|
//! | C1 | production `provision_repo_at`, correct Gateway capability | 401 then authorized; clean origin; zero persisted credential bytes; no staging residue |
//! | C2 | production path, wrong capability | refused; error and tree carry no credential |
//! | NC | raw `git clone` with the capability embedded in the URL | the same oracle finds the persisted credential |
use super::*;
use std::sync::Mutex;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

const CAPABILITY: &str = "t29-gateway-capability-5c0ffee";

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut encoded = String::new();
    for chunk in bytes.chunks(3) {
        let value = chunk.iter().enumerate().fold(0u32, |value, (index, byte)| {
            value | (u32::from(*byte) << (16 - 8 * index))
        });
        for index in 0..4 {
            if index <= chunk.len() {
                encoded.push(TABLE[((value >> (18 - 6 * index)) & 0x3f) as usize] as char);
            } else {
                encoded.push('=');
            }
        }
    }
    encoded
}

fn basic(capability: &str) -> String {
    format!("Basic {}", base64(format!("git:{capability}").as_bytes()))
}

/// Requests the Gateway saw: the Authorization header of each, if any.
type Seen = std::sync::Arc<Mutex<Vec<Option<String>>>>;

struct Gateway {
    url: String,
    seen: Seen,
}

async fn read_request(
    stream: &mut tokio::net::TcpStream,
) -> Option<(String, Vec<(String, String)>, Vec<u8>)> {
    let mut buffer = Vec::new();
    let header_end = loop {
        let mut chunk = [0; 4096];
        let read = stream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break end;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?.to_owned();
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect::<Vec<_>>();
    let mut body = buffer[header_end + 4..].to_vec();
    let header = |name: &str| {
        headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    };
    if let Some(length) = header("content-length").and_then(|value| value.parse::<usize>().ok()) {
        while body.len() < length {
            let mut chunk = vec![0; length - body.len()];
            let read = stream.read(&mut chunk).await.ok()?;
            if read == 0 {
                return None;
            }
            body.extend_from_slice(&chunk[..read]);
        }
    } else if header("transfer-encoding").is_some_and(|value| value.eq_ignore_ascii_case("chunked"))
    {
        // Decode until the terminating zero-length chunk.
        let mut decoded = Vec::new();
        loop {
            while !body.windows(2).any(|window| window == b"\r\n") {
                let mut chunk = [0; 4096];
                let read = stream.read(&mut chunk).await.ok()?;
                if read == 0 {
                    return None;
                }
                body.extend_from_slice(&chunk[..read]);
            }
            let line_end = body.windows(2).position(|window| window == b"\r\n")?;
            let size = usize::from_str_radix(String::from_utf8_lossy(&body[..line_end]).trim(), 16)
                .ok()?;
            while body.len() < line_end + 2 + size + 2 {
                let mut chunk = [0; 4096];
                let read = stream.read(&mut chunk).await.ok()?;
                if read == 0 {
                    return None;
                }
                body.extend_from_slice(&chunk[..read]);
            }
            decoded.extend_from_slice(&body[line_end + 2..line_end + 2 + size]);
            body.drain(..line_end + 2 + size + 2);
            if size == 0 {
                break;
            }
        }
        body = decoded;
    }
    Some((request_line, headers, body))
}

async fn serve(
    mut stream: tokio::net::TcpStream,
    project_root: PathBuf,
    expected: String,
    seen: Seen,
) {
    let Some((request_line, headers, body)) = read_request(&mut stream).await else {
        return;
    };
    let header = |name: &str| {
        headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    };
    let authorization = header("authorization");
    seen.lock().unwrap().push(authorization.clone());
    if authorization.as_deref() != Some(expected.as_str()) {
        let _ = stream
            .write_all(b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"t29\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await;
        return;
    }
    let mut parts = request_line.split(' ');
    let method = parts.next().unwrap_or_default().to_owned();
    let target = parts.next().unwrap_or_default().to_owned();
    let (path, query) = target.split_once('?').unwrap_or((target.as_str(), ""));
    let mut cgi = tokio::process::Command::new("git");
    cgi.arg("http-backend")
        .env("GIT_PROJECT_ROOT", &project_root)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("REQUEST_METHOD", &method)
        .env("PATH_INFO", path)
        .env("QUERY_STRING", query)
        .env("REMOTE_ADDR", "127.0.0.1")
        .env("REMOTE_USER", "git")
        .env("CONTENT_LENGTH", body.len().to_string())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    for (name, variable) in [
        ("content-type", "CONTENT_TYPE"),
        ("git-protocol", "HTTP_GIT_PROTOCOL"),
        ("content-encoding", "HTTP_CONTENT_ENCODING"),
    ] {
        if let Some(value) = header(name) {
            cgi.env(variable, value);
        }
    }
    let mut child = cgi.spawn().expect("spawn git http-backend");
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(&body).await.unwrap();
    drop(stdin);
    let output = child.wait_with_output().await.unwrap();
    let raw = output.stdout;
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|end| (end, 4))
        .or_else(|| {
            raw.windows(2)
                .position(|window| window == b"\n\n")
                .map(|end| (end, 2))
        })
        .expect("CGI response headers");
    let cgi_headers = String::from_utf8_lossy(&raw[..split.0]).into_owned();
    let cgi_body = &raw[split.0 + split.1..];
    let mut status = "200 OK".to_owned();
    let mut response = Vec::new();
    for line in cgi_headers.lines() {
        match line.split_once(':') {
            Some((name, value)) if name.eq_ignore_ascii_case("status") => {
                status = value.trim().to_owned()
            }
            Some(_) => {
                response.extend_from_slice(line.trim_end().as_bytes());
                response.extend_from_slice(b"\r\n");
            }
            None => {}
        }
    }
    let mut message = format!("HTTP/1.1 {status}\r\n").into_bytes();
    message.extend_from_slice(&response);
    message.extend_from_slice(
        format!(
            "Content-Length: {}\r\nConnection: close\r\n\r\n",
            cgi_body.len()
        )
        .as_bytes(),
    );
    message.extend_from_slice(cgi_body);
    let _ = stream.write_all(&message).await;
}

async fn gateway(project_root: &Path, expected_capability: &str) -> Gateway {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/repo.git", listener.local_addr().unwrap());
    let seen: Seen = Default::default();
    let expected = basic(expected_capability);
    let (root, recorded) = (project_root.to_owned(), seen.clone());
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            tokio::spawn(serve(
                stream,
                root.clone(),
                expected.clone(),
                recorded.clone(),
            ));
        }
    });
    Gateway { url, seen }
}

fn bare_fixture(directory: &Path) {
    let work = directory.join("work");
    std::fs::create_dir_all(&work).unwrap();
    run_git(Some(&work), &["init", "-q"]).unwrap();
    std::fs::write(work.join("a.txt"), b"credentialed fixture").unwrap();
    run_git(Some(&work), &["add", "a.txt"]).unwrap();
    run_git(
        Some(&work),
        &[
            "-c",
            "user.name=fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "fixture",
        ],
    )
    .unwrap();
    let bare = directory.join("project/repo.git");
    run_git(
        None,
        &[
            "clone",
            "-q",
            "--bare",
            work.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    )
    .unwrap();
}

/// O3 oracle: every persisted file beneath `root` (including `.git`), never
/// following a link, that contains the capability or its Basic encoding.
fn credential_traces(root: &Path, capability: &str) -> Vec<PathBuf> {
    let needles = [
        capability.as_bytes().to_vec(),
        base64(format!("git:{capability}").as_bytes()).into_bytes(),
    ];
    let mut traces = Vec::new();
    let mut pending = vec![root.to_owned()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else {
            continue;
        };
        for entry in entries {
            let path = entry.unwrap().path();
            let metadata = std::fs::symlink_metadata(&path).unwrap();
            if metadata.is_dir() {
                pending.push(path);
            } else if metadata.is_file() {
                let bytes = std::fs::read(&path).unwrap();
                if needles.iter().any(|needle| {
                    bytes
                        .windows(needle.len())
                        .any(|window| window == needle.as_slice())
                }) {
                    traces.push(path);
                }
            }
        }
    }
    traces
}

async fn provision(root: &Path, url: &str, capability: &str) -> Result<(), SandboxError> {
    let (root, url, capability) = (root.to_owned(), url.to_owned(), capability.to_owned());
    tokio::time::timeout(
        std::time::Duration::from_secs(120),
        tokio::task::spawn_blocking(move || {
            provision_repo_at(
                &IsolatedRoot::new(root),
                "workspace/repo",
                &url,
                None,
                None,
                Some(&pc::RepositoryHttpBasicCredential::gateway_capability(
                    capability,
                )),
                pc::RepositoryRealizationIntent::Initialize,
            )
        }),
    )
    .await
    .expect("credentialed clone never finished")
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn credentialed_clone_uses_the_capability_on_the_wire_but_never_persists_or_logs_it() {
    let tmp = tempfile::tempdir().unwrap();
    bare_fixture(tmp.path());
    let server = gateway(&tmp.path().join("project"), CAPABILITY).await;

    // C1: the production clone authenticates through its command-scoped helper.
    let root = tmp.path().join("root");
    std::fs::create_dir_all(&root).unwrap();
    provision(&root, &server.url, CAPABILITY)
        .await
        .unwrap_or_else(|error| panic!("C1 credentialed clone: {error}"));
    let seen = server.seen.lock().unwrap().clone();
    assert!(
        seen.contains(&None),
        "C1 the first request is anonymous (401)"
    );
    assert!(
        seen.contains(&Some(basic(CAPABILITY))),
        "C1 the capability is actually presented on the wire: {seen:?}"
    );
    let repository = root.join("workspace/repo");
    assert_eq!(
        std::fs::read(repository.join("a.txt")).unwrap(),
        b"credentialed fixture"
    );
    assert_eq!(
        git_stdout(Some(&repository), &["remote", "get-url", "origin"])
            .unwrap()
            .trim(),
        server.url,
        "C1/O3 origin is the clean URL"
    );
    assert_eq!(
        credential_traces(&root, CAPABILITY),
        Vec::<PathBuf>::new(),
        "C1/O3 no persisted credential anywhere under the root"
    );
    let siblings = std::fs::read_dir(root.join("workspace"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    assert_eq!(siblings, ["repo"], "C1 no staging residue beside the mount");

    // C2: a refused capability leaves no credential in the error or the tree.
    let refused_root = tmp.path().join("refused");
    std::fs::create_dir_all(&refused_root).unwrap();
    let wrong = "t29-wrong-capability-0badf00d";
    let error = provision(&refused_root, &server.url, wrong)
        .await
        .expect_err("C2 the Gateway refuses a wrong capability")
        .to_string();
    for secret in [wrong.to_owned(), base64(format!("git:{wrong}").as_bytes())] {
        assert!(
            !error.contains(&secret),
            "C2/O3 error carries no credential: {error}"
        );
    }
    assert!(
        !refused_root.join("workspace/repo").exists(),
        "C2 nothing published"
    );
    assert_eq!(
        credential_traces(&refused_root, wrong),
        Vec::<PathBuf>::new(),
        "C2/O3"
    );

    // NC: the same oracle over a URL-embedded credential clone detects it.
    let embedded = tmp.path().join("embedded");
    let url_with_secret = server
        .url
        .replacen("http://", &format!("http://git:{CAPABILITY}@"), 1);
    let destination = embedded.to_str().unwrap().to_owned();
    let status = tokio::task::spawn_blocking(move || {
        std::process::Command::new("git")
            .env("GIT_TERMINAL_PROMPT", "0")
            .args([
                "-c",
                "credential.helper=",
                "clone",
                "-q",
                &url_with_secret,
                &destination,
            ])
            .status()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(
        status.success(),
        "NC the embedded-credential clone itself succeeds"
    );
    assert!(
        !credential_traces(&embedded, CAPABILITY).is_empty(),
        "NC the oracle must detect a persisted credential"
    );
}
