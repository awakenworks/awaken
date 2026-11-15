// @formal surface exclude=test_only
// @formal owner crates/platform/execution/awaken-session-mcp/src/mcp/attachment.rs
// @formal reason: Decision table of the MCP credential-refresh material and its terminal refusal class.
//! The refresh material one resolved MCP bearer carries, by realization
//! mechanism, the host's refresh port and the pin's refresh configuration.

use std::sync::{Arc, Mutex};

use awaken_agent_contract::credential::{
    AuthChallenge, Credential, CredentialAccess, CredentialExecutionPolicy,
    CredentialMaterialSource, CredentialRef, CredentialRefreshAccess, CredentialRefresher,
    CredentialUsage, ModelExposurePolicy, PlaintextBoundary, PlaintextHolder, TokenEndpointAuth,
};
use awaken_credential_contract::CredentialRealizationKind;
use awaken_session_configuration_contract::mcp::McpAttachmentId;
use awaken_session_configuration_contract::mcp::{
    McpCredentialRefresh, McpGeneration, McpGenerationRef,
};
use awaken_session_execution_contract::{
    SessionRealizationControlDisposition, effect_failure_disposition,
};

use super::{MCP_CREDENTIAL_REFRESH_UNAVAILABLE_CODE, mcp_refresh_material};

struct NoRefresh;

#[async_trait::async_trait]
impl CredentialRefresher for NoRefresh {
    async fn refresh(&self, _challenge: &AuthChallenge) -> Option<Credential> {
        None
    }
}

/// Records which generation and pin each refresher was built for.
#[derive(Default)]
struct RecordingPort(Mutex<Vec<String>>);

impl McpCredentialRefresh for RecordingPort {
    fn refresher(
        &self,
        generation: &McpGenerationRef,
        credential: &CredentialAccess,
    ) -> Arc<dyn CredentialRefresher> {
        self.0.lock().unwrap().push(format!(
            "{}:{}:{}:{}",
            generation.session_id().as_str(),
            credential.credential.id,
            credential.credential.revision,
            credential.refresh.is_some()
        ));
        Arc::new(NoRefresh)
    }
}

fn refresh() -> CredentialRefreshAccess {
    CredentialRefreshAccess::new(
        7,
        "https://issuer.example/token".into(),
        "cli-1".into(),
        TokenEndpointAuth::None,
        None,
        "refresh-ref".into(),
        "access-ref".into(),
        None,
        None,
    )
}

fn pin(refresh: Option<CredentialRefreshAccess>) -> CredentialAccess {
    let mut access = CredentialAccess::new(
        CredentialRef {
            id: "cred-1".into(),
            revision: 7,
        },
        CredentialMaterialSource::ControlPlaneReference,
        CredentialUsage::HttpHeader {
            name: "authorization".into(),
            scheme: Some("Bearer".into()),
        },
        CredentialExecutionPolicy::exact(
            PlaintextHolder::new(PlaintextBoundary::Worker, "awaken.worker"),
            ModelExposurePolicy::Forbidden,
        ),
    );
    access.refresh = refresh;
    access
}

fn generation() -> McpGenerationRef {
    McpGenerationRef::from_persisted(
        awaken_session_types::SessionId::from("session-1"),
        McpGeneration::of_bare_target(
            McpAttachmentId("docs".into()),
            1,
            &awaken_agent_contract::McpTarget::parse_http("https://docs.example.test").unwrap(),
        ),
        "worker/boot".into(),
        2,
        10_000,
    )
    .expect("nonzero epoch")
}

#[test]
fn mcp_refresh_material_follows_its_decision_table() {
    let generation = generation();
    let built = |kind, refresh: Option<CredentialRefreshAccess>, with_port: bool| {
        let port = RecordingPort::default();
        let result = mcp_refresh_material(
            kind,
            &generation,
            &pin(refresh),
            with_port.then_some(&port as &dyn McpCredentialRefresh),
        );
        let calls = port.0.lock().unwrap().clone();
        (result, calls)
    };

    // M1: a mechanism other than WorkerRelay refreshes nothing on this Worker.
    for kind in [
        CredentialRealizationKind::PlatformRelay,
        CredentialRealizationKind::ProcessProtocolField,
    ] {
        for configured in [None, Some(refresh())] {
            for with_port in [false, true] {
                let (result, calls) = built(kind, configured.clone(), with_port);
                assert!(result.unwrap().is_none(), "M1 {kind:?}");
                assert!(calls.is_empty(), "M1 {kind:?} builds nothing");
            }
        }
    }
    let relay = CredentialRealizationKind::WorkerRelay;
    // M2: with the port, its refresher for this generation's exact pin, with
    // or without a refresh configuration (the port decides exchange/reload).
    for (configured, expected) in [
        (Some(refresh()), "session-1:cred-1:7:true"),
        (None, "session-1:cred-1:7:false"),
    ] {
        let (result, calls) = built(relay, configured, true);
        assert!(result.unwrap().is_some(), "M2");
        assert_eq!(calls, [expected], "M2");
    }
    // M3: a configured refresh without the port is a terminal refusal.
    let (result, calls) = built(relay, Some(refresh()), false);
    let Err(error) = result else {
        panic!("M3 a missing refresh capability is refused");
    };
    assert_eq!(error.code, MCP_CREDENTIAL_REFRESH_UNAVAILABLE_CODE, "M3");
    assert_eq!(
        effect_failure_disposition(&error),
        SessionRealizationControlDisposition::Terminal,
        "M3 no retry can add the capability, so none spends the budget"
    );
    assert!(calls.is_empty(), "M3");
    // M4: neither: the bearer is used as resolved.
    let (result, calls) = built(relay, None, false);
    assert!(result.unwrap().is_none(), "M4");
    assert!(calls.is_empty(), "M4");
}
