// @formal surface exclude=test_only
// @formal owner crates/server/awaken-session-mcp/src/mcp.rs
// @formal reason: Transport-fault classification table, its Run projection and a text-classifier negative control.
//! T33-C01: MCP transport faults are classified by their typed origin and the
//! classification survives the Run boundary. Human-readable text never
//! selects a class.

use super::error::run_error;
use super::{McpErrorKind, mcp_transport_error};
use awaken_ext_mcp::McpTransportError;
use awaken_runtime_contract::runs::RunErrorKind;

/// One row: a typed fault whose text deliberately names another class.
struct Row {
    rule: &'static str,
    source: fn() -> McpTransportError,
    kind: McpErrorKind,
    run_kind: RunErrorKind,
    code: &'static str,
}

fn rows() -> [Row; 5] {
    [
        Row {
            rule: "C1 transport fault mentioning an auth challenge",
            source: || McpTransportError::TransportError("auth challenge: HTTP 401".into()),
            kind: McpErrorKind::Unavailable,
            run_kind: RunErrorKind::Unavailable,
            code: "mcp_connection_failed",
        },
        Row {
            rule: "C2 timeout mentioning an auth challenge",
            source: || McpTransportError::Timeout("auth challenge: 30s".into()),
            kind: McpErrorKind::Unavailable,
            run_kind: RunErrorKind::Unavailable,
            code: "mcp_connection_failed",
        },
        Row {
            rule: "C3 protocol fault mentioning an auth challenge",
            source: || McpTransportError::ProtocolError("auth challenge: HTTP 401".into()),
            kind: McpErrorKind::Internal,
            run_kind: RunErrorKind::Internal,
            code: "mcp_protocol_failed",
        },
        Row {
            rule: "C4 connection closed",
            source: || McpTransportError::ConnectionClosed,
            kind: McpErrorKind::Internal,
            run_kind: RunErrorKind::Internal,
            code: "mcp_protocol_failed",
        },
        Row {
            rule: "C5 server payload naming a connection timeout",
            source: || McpTransportError::ServerError("timeout: connection refused".into()),
            kind: McpErrorKind::Internal,
            run_kind: RunErrorKind::Internal,
            code: "mcp_protocol_failed",
        },
    ]
}

/// The oracle check, parameterized by the classifier under test. Returns the
/// rules whose MCP or Run classification is not the expected one.
fn misclassified(classify: impl Fn(McpTransportError) -> super::McpError) -> Vec<&'static str> {
    rows()
        .into_iter()
        .filter(|row| {
            let error = classify((row.source)());
            let kind = error.kind;
            let code = error.code.clone();
            let run = run_error(error);
            kind != row.kind || code != row.code || run.kind != row.run_kind || run.code != row.code
        })
        .map(|row| row.rule)
        .collect()
}

#[test]
fn transport_fault_classes_follow_the_typed_origin_across_the_run_boundary() {
    assert_eq!(
        misclassified(|source| mcp_transport_error("docs", "https://mcp.example", source)),
        Vec::<&str>::new(),
        "production classification is by variant"
    );

    // NC: a classifier that reads the rendered message. The same oracle must
    // reject it on the rows whose text names another class.
    let text_mutant = |source: McpTransportError| {
        let message = source.to_string();
        if message.contains("auth challenge") {
            super::McpError::classified("mcp_authentication_failed", message)
        } else if message.contains("timeout") || message.contains("connection") {
            super::McpError::unavailable_classified("mcp_connection_failed", message)
        } else {
            super::McpError::classified("mcp_protocol_failed", message)
        }
    };
    let detected = misclassified(text_mutant);
    assert!(
        detected.contains(&"C1 transport fault mentioning an auth challenge")
            && detected.contains(&"C3 protocol fault mentioning an auth challenge")
            && detected.contains(&"C5 server payload naming a connection timeout"),
        "NC the text classifier is detected: {detected:?}"
    );
}

/// A real 401 from the in-process MCP server reaches the materialized
/// connection as a permanent authentication failure, not a retryable one.
#[tokio::test]
async fn a_real_auth_challenge_is_a_permanent_authentication_failure() {
    let (url, seen) = crate::test_mcp::start(Some("Bearer expected-token")).await;
    let staged = [crate::mcp_material::McpTransportMaterial {
        name: "docs".into(),
        prompts_as_skills: false,
        transport: crate::mcp_material::McpTransportMaterialKind::Http {
            url,
            bearer: Some(awaken_agent_contract::RedactedString::new("wrong-token")),
            refresh: None,
        },
    }];
    let Err(error) = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        super::connect_materialized(&staged),
    )
    .await
    .expect("the refused connection settles") else {
        panic!("a refused bearer must not connect");
    };
    assert_eq!(error.code, "mcp_authentication_failed");
    assert_eq!(error.kind, McpErrorKind::Internal);
    let run = run_error(error);
    assert_eq!(run.kind, RunErrorKind::Internal, "not retryable at the Run");
    assert_eq!(run.code, "mcp_authentication_failed");
    assert!(seen.lock().is_empty(), "no MCP method was admitted");
}

#[test]
fn server_payload_text_cannot_select_the_authentication_class() {
    // A1 an untrusted server payload imitates challenge text -> protocol fault;
    // A2 a transport-authored challenge with arbitrary wording -> auth fault.
    // Classification follows the typed origin, independently of either text.
    let error = mcp_transport_error(
        "docs",
        "https://mcp.example",
        McpTransportError::ServerError("auth challenge: upstream tool said so".into()),
    );
    assert_eq!(
        error.code, "mcp_protocol_failed",
        "only a typed auth challenge may select mcp_authentication_failed"
    );
    let challenge = mcp_transport_error(
        "docs",
        "https://mcp.example",
        McpTransportError::AuthenticationRequired("credentials rejected".into()),
    );
    assert_eq!(challenge.code, "mcp_authentication_failed", "A2");
}
