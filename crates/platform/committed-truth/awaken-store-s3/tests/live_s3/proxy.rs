use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Request, Response, body::Incoming, service::service_fn};
use hyper_util::rt::TokioIo;
use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
    },
};
use tokio::{net::TcpListener, sync::Notify};

pub(super) const BEFORE: u8 = 1;
pub(super) const AFTER: u8 = 2;
pub(super) const HOLD_AFTER: u8 = 3;
#[derive(Default)]
pub(super) struct Fault {
    mode: AtomicU8,
    upload: AtomicBool,
    pub(super) before: AtomicU64,
    pub(super) after: AtomicU64,
    pub(super) persisted: Notify,
    pub(super) release: Notify,
}
pub(super) struct Proxy {
    pub(super) endpoint: String,
    pub(super) fault: Arc<Fault>,
    server: tokio::task::JoinHandle<()>,
}
impl Drop for Proxy {
    fn drop(&mut self) {
        self.server.abort();
    }
}
fn wire_error() -> io::Error {
    io::Error::new(io::ErrorKind::ConnectionReset, "injected S3 response loss")
}
impl Proxy {
    pub(super) async fn new(upstream: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let fault = Arc::new(Fault::default());
        let shared = fault.clone();
        let server = tokio::spawn(async move {
            let client = reqwest::Client::builder()
                .http1_only()
                .no_proxy()
                .build()
                .unwrap();
            let mut connections = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (stream, _) = accepted.unwrap();
                        let upstream = upstream.clone();
                        let fault = shared.clone();
                        let client = client.clone();
                        connections.spawn(async move {
                            // A service error closes TCP before any response headers.
                            let service = service_fn(move |request| forward(request, upstream.clone(), client.clone(), fault.clone()));
                            let _closed_or_completed = hyper::server::conn::http1::Builder::new()
                                .serve_connection(TokioIo::new(stream), service).await;
                        });
                    }
                    _ = connections.join_next(), if !connections.is_empty() => {}
                }
            }
        });
        Self {
            endpoint,
            fault,
            server,
        }
    }
    pub(super) fn arm(&self, mode: u8) {
        self.fault.upload.store(false, Ordering::SeqCst);
        self.fault.mode.store(mode, Ordering::SeqCst);
    }
    pub(super) fn arm_upload(&self, mode: u8) {
        self.fault.upload.store(true, Ordering::SeqCst);
        self.fault.mode.store(mode, Ordering::SeqCst);
    }
    pub(super) async fn wait_persisted(&self) {
        tokio::time::timeout(
            std::time::Duration::from_secs(120),
            self.fault.persisted.notified(),
        )
        .await
        .expect("real S3 must acknowledge the held conditional PUT");
    }
}
async fn forward(
    request: Request<Incoming>,
    upstream: String,
    client: reqwest::Client,
    fault: Arc<Fault>,
) -> Result<Response<Full<Bytes>>, io::Error> {
    let targeted = request.method() == hyper::Method::PUT
        && if fault.upload.load(Ordering::SeqCst) {
            request.uri().path().contains("/segments/")
        } else {
            request.uri().path().ends_with("/head.json")
        };
    let mode = if targeted {
        fault.mode.swap(0, Ordering::SeqCst)
    } else {
        0
    };
    if mode == BEFORE {
        fault.before.fetch_add(1, Ordering::SeqCst);
        return Err(wire_error());
    }
    let (parts, incoming) = request.into_parts();
    let body = incoming
        .collect()
        .await
        .map_err(|_| wire_error())?
        .to_bytes();
    let target = format!(
        "{}{}",
        upstream.trim_end_matches('/'),
        parts
            .uri
            .path_and_query()
            .map(|p| p.as_str())
            .unwrap_or("/")
    );
    let mut headers = parts.headers;
    headers.remove(hyper::header::CONNECTION);
    headers.remove(hyper::header::TRANSFER_ENCODING);
    // Preserve the signed Host header and exact PUT body.
    let response = client
        .request(parts.method, target)
        .headers(headers)
        .body(body)
        .send()
        .await
        .map_err(|_| wire_error())?;
    let status = response.status();
    let mut headers = response.headers().clone();
    headers.remove(hyper::header::CONNECTION);
    headers.remove(hyper::header::TRANSFER_ENCODING);
    let body = response.bytes().await.map_err(|_| wire_error())?;
    if matches!(mode, AFTER | HOLD_AFTER) && status.is_success() {
        fault.after.fetch_add(1, Ordering::SeqCst);
        fault.persisted.notify_one();
        if mode == HOLD_AFTER {
            fault.release.notified().await;
        }
        return Err(wire_error());
    }
    let mut response = Response::new(Full::new(body));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    Ok(response)
}
