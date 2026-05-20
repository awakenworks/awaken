//! `RemoteEndpoint` + `RemoteAuth::bearer` — pins the A2A delegate shape
//! `reference/protocols/a2a.md` cites.

use awaken::registry_spec::{RemoteAuth, RemoteEndpoint};

fn main() {
    let auth = RemoteAuth::bearer("secret-token-123");
    assert_eq!(auth.auth_type, "bearer");
    assert_eq!(auth.param_str("token"), Some("secret-token-123"));

    let endpoint = RemoteEndpoint {
        backend: "a2a".into(),
        base_url: "https://agent.example.com".into(),
        auth: Some(auth),
        target: Some("worker-agent".into()),
        timeout_ms: 30_000,
        options: Default::default(),
    };

    let json = serde_json::to_value(&endpoint).expect("encode");
    // Flat `type: bearer` field on auth (serde rename = "type").
    assert_eq!(json["auth"]["type"], "bearer");

    let parsed: RemoteEndpoint = serde_json::from_value(json).expect("decode");
    assert_eq!(parsed.backend, "a2a");
    assert_eq!(parsed.target.as_deref(), Some("worker-agent"));
    assert_eq!(parsed.timeout_ms, 30_000);
}
