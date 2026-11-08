// @formal surface exclude=test_only
// @formal owner crates/platform/execution/awaken-session-mcp/src/mcp/attachment.rs
// @formal reason: Decision table of the MCP credential-refresh material and its terminal refusal class.
//! The refresh material one resolved MCP bearer carries, by realization
//! mechanism, refresh configuration and the Worker's refresh capability.

use std::sync::{Arc, Mutex};

use awaken_agent_contract::credential::{
    AuthChallenge, Credential, CredentialRef, CredentialRefreshAccess, CredentialRefresher,
    TokenEndpointAuth,
};
use awaken_credential_contract::{
    CredentialRealizationKind, CredentialRefreshFactory, CredentialSourceId,
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

/// Records which factory method built a refresher, for which credential.
#[derive(Default)]
struct RecordingFactory(Mutex<Vec<String>>);

impl CredentialRefreshFactory for RecordingFactory {
    fn refresher(
        &self,
        credential_id: CredentialSourceId,
        access: CredentialRefreshAccess,
    ) -> Arc<dyn CredentialRefresher> {
        self.0.lock().unwrap().push(format!(
            "refresher:{}:{}",
            credential_id.0, access.credential_revision
        ));
        Arc::new(NoRefresh)
    }

    fn bearer_reloader(
        &self,
        credential_id: CredentialSourceId,
        credential_revision: u64,
    ) -> Arc<dyn CredentialRefresher> {
        self.0.lock().unwrap().push(format!(
            "reloader:{}:{credential_revision}",
            credential_id.0
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

#[test]
fn mcp_refresh_material_follows_its_decision_table() {
    let credential = CredentialRef {
        id: "cred-1".into(),
        revision: 7,
    };
    let refresh = refresh();
    let built = |kind, refresh: Option<&CredentialRefreshAccess>, with_factory: bool| {
        let factory = RecordingFactory::default();
        let result = mcp_refresh_material(
            kind,
            &credential,
            refresh,
            with_factory.then_some(&factory as &dyn CredentialRefreshFactory),
        );
        let calls = factory.0.lock().unwrap().clone();
        (result, calls)
    };

    // M1: a mechanism other than WorkerRelay refreshes nothing on this Worker.
    for kind in [
        CredentialRealizationKind::PlatformRelay,
        CredentialRealizationKind::ProcessProtocolField,
    ] {
        for configured in [None, Some(&refresh)] {
            for with_factory in [false, true] {
                let (result, calls) = built(kind, configured, with_factory);
                assert!(result.unwrap().is_none(), "M1 {kind:?}");
                assert!(calls.is_empty(), "M1 {kind:?} builds nothing");
            }
        }
    }
    let relay = CredentialRealizationKind::WorkerRelay;
    // M2: a configured refresh with the capability exchanges the refresh token.
    let (result, calls) = built(relay, Some(&refresh), true);
    assert!(result.unwrap().is_some(), "M2");
    assert_eq!(calls, ["refresher:cred-1:7"], "M2");
    // M3: a configured refresh without the capability is a terminal refusal.
    let (result, calls) = built(relay, Some(&refresh), false);
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
    // M4: an unconfigured refresh with the capability reloads the bearer.
    let (result, calls) = built(relay, None, true);
    assert!(result.unwrap().is_some(), "M4");
    assert_eq!(calls, ["reloader:cred-1:7"], "M4");
    // M5: neither: the bearer is used as resolved.
    let (result, calls) = built(relay, None, false);
    assert!(result.unwrap().is_none(), "M5");
    assert!(calls.is_empty(), "M5");
}
