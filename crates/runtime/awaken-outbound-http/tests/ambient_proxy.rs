//! The ambient-proxy rule is a property of the client type, proved against a
//! real `HTTP_PROXY` and a real proxy listener.
//!
//! This lives in its own test binary because it mutates process environment
//! before any client is built, and `reqwest` reads the proxy environment once
//! per client construction.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use awaken_outbound_http::{ExternalHttpClient, InternalHttpClient, JsonRequest};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Record the request line of everything that reaches this listener and answer
/// 204, so a proxied request is distinguishable from an unproxied one by its
/// absolute-form request target.
async fn spawn_probe(seen: Arc<Mutex<Vec<String>>>) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let seen = seen.clone();
            tokio::spawn(async move {
                let mut buffer = vec![0_u8; 2048];
                let Ok(read) = stream.read(&mut buffer).await else {
                    return;
                };
                let request = String::from_utf8_lossy(&buffer[..read]).to_string();
                if let Some(line) = request.lines().next() {
                    seen.lock().unwrap().push(line.to_owned());
                }
                let _ = stream
                    .write_all(b"HTTP/1.1 204 No Content\r\nconnection: close\r\n\r\n")
                    .await;
                let _ = stream.flush().await;
            });
        }
    });
    address
}

#[tokio::test(flavor = "current_thread")]
async fn the_client_type_decides_whether_an_ambient_proxy_applies() {
    // Cause/effect decision table: C1 an ambient `HTTP_PROXY` is exported;
    // C2 the caller holds an `InternalHttpClient` (private control plane);
    // C3 the caller holds an `ExternalHttpClient` for a routable target;
    // C4 the caller holds an `ExternalHttpClient` for a loopback target.
    // Effects: E1 the request goes straight to the target; E2 the request goes
    // through the proxy.
    //
    // | Rule | C1 | C2 | C3 | C4 | Effect |
    // | P1   | T  | T  | F  | F  | E1     |
    // | P2   | T  | F  | T  | F  | E2     |
    // | P3   | T  | F  | F  | T  | E1     |
    //
    // P1 is the rule that was carried by hand at seven control-plane call sites
    // and missed at the eighth: a workstation or sidecar proxy variable must not
    // silently redirect private control traffic.
    let proxied = Arc::new(Mutex::new(Vec::new()));
    let direct = Arc::new(Mutex::new(Vec::new()));
    let proxy = spawn_probe(proxied.clone()).await;
    let origin = spawn_probe(direct.clone()).await;

    // SAFETY: this test binary contains exactly one test and runs it on a single
    // thread, so no other thread can be reading the environment concurrently.
    unsafe {
        std::env::set_var("HTTP_PROXY", format!("http://{proxy}"));
        std::env::set_var("http_proxy", format!("http://{proxy}"));
    }

    let internal = InternalHttpClient::new(Duration::from_secs(2)).unwrap();
    let reply = internal
        .send(&JsonRequest::get(format!("http://{origin}/internal")))
        .await
        .expect("P1 reaches the control plane directly");
    assert_eq!(reply.status(), 204, "P1");
    assert_eq!(
        direct.lock().unwrap().clone(),
        vec!["GET /internal HTTP/1.1".to_owned()],
        "P1 origin-form request means no proxy hop"
    );
    assert!(proxied.lock().unwrap().is_empty(), "P1 proxy untouched");

    let external =
        ExternalHttpClient::for_target("http://third-party.invalid", Duration::from_secs(2))
            .unwrap();
    let _ = external
        .send(&JsonRequest::get("http://third-party.invalid/tool"))
        .await;
    assert_eq!(
        proxied.lock().unwrap().clone(),
        vec!["GET http://third-party.invalid/tool HTTP/1.1".to_owned()],
        "P2 absolute-form request means the configured proxy is in force"
    );

    let emulator =
        ExternalHttpClient::for_target(&format!("http://{origin}"), Duration::from_secs(2))
            .unwrap();
    let reply = emulator
        .send(&JsonRequest::get(format!("http://{origin}/emulator")))
        .await
        .expect("P3 reaches the on-host emulator directly");
    assert_eq!(reply.status(), 204, "P3");
    assert_eq!(
        direct.lock().unwrap().len(),
        2,
        "P3 the loopback emulator is reached without a proxy hop"
    );
    assert_eq!(proxied.lock().unwrap().len(), 1, "P3 proxy untouched");
}
