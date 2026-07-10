//! `resolve_mcp_servers` (ADR-0043 Phase 3): authored [`McpServerDef`]s become
//! injection-ready [`ResolvedMcpServer`]s — `None` binding yields no credential,
//! `Exact` materializes one source, a pool binding fails over member-by-member,
//! and an unmaterializable def fails the whole resolution (fail-closed).

use std::collections::HashMap;

use awaken_agent_contract::RedactedString;
use awaken_config_resolver::{
    McpServerDef, McpServerId, ResolveError, SourceLookup, resolve_mcp_servers,
};
use awaken_credential_vault::repo::{CredentialRepo, InMemoryCredentialRepo, enter_credential};
use awaken_credential_vault::{
    CredentialBinding, CredentialCreateParams, CredentialKind, CredentialPool, CredentialPoolId,
    CredentialPoolMember, CredentialSource, CredentialSourceId, CredentialStatus,
    InMemorySecretStore,
};

/// A lookup exposing both individual sources and one pool.
struct PoolCtx {
    sources: HashMap<String, CredentialSource>,
    pool: CredentialPool,
}

impl SourceLookup for PoolCtx {
    fn get(&self, id: &str) -> Option<&CredentialSource> {
        self.sources.get(id)
    }
    fn get_pool(&self, id: &str) -> Option<&CredentialPool> {
        (self.pool.id.0 == id).then_some(&self.pool)
    }
}

fn def(id: &str, binding: CredentialBinding) -> McpServerDef {
    McpServerDef {
        id: McpServerId(id.into()),
        display_name: format!("{id} display"),
        url: format!("https://{id}.example/mcp"),
        credential_binding: binding,
        version: 1,
    }
}

fn disabled_source(id: &str) -> CredentialSource {
    CredentialSource {
        id: CredentialSourceId(id.into()),
        workspace_id: "ws".into(),
        kind: CredentialKind::Vault,
        provider_id: None,
        env_key: None,
        material_ref: None,
        oauth_command: None,
        status: CredentialStatus::Disabled,
        version: 1,
    }
}

async fn enter(store: &InMemorySecretStore, repo: &InMemoryCredentialRepo, secret: &str) -> String {
    let src = enter_credential(
        CredentialCreateParams {
            workspace_id: "ws".into(),
            kind: CredentialKind::Vault,
            provider_id: None,
            env_key: None,
            secret: Some(RedactedString::new(secret)),
        },
        store,
        repo,
    )
    .await
    .unwrap();
    src.id.0
}

#[tokio::test]
async fn none_binding_yields_a_credential_free_server() {
    let store = InMemorySecretStore::new();
    let sources: HashMap<String, CredentialSource> = HashMap::new();

    let resolved = resolve_mcp_servers(&[def("docs", CredentialBinding::None)], &sources, &store)
        .await
        .expect("a None binding resolves without a credential");

    assert_eq!(resolved.len(), 1);
    assert_eq!(resolved[0].name, "docs display");
    assert_eq!(resolved[0].url, "https://docs.example/mcp");
    assert!(resolved[0].credential.is_none());
}

#[tokio::test]
async fn exact_binding_materializes_the_named_source() {
    let store = InMemorySecretStore::new();
    let repo = InMemoryCredentialRepo::new();
    let good = enter(&store, &repo, "sk-mcp-secret").await;
    let mut sources: HashMap<String, CredentialSource> = HashMap::new();
    sources.insert(
        good.clone(),
        repo.get(&CredentialSourceId(good.clone())).await.unwrap(),
    );

    let resolved = resolve_mcp_servers(
        &[def(
            "jira",
            CredentialBinding::Exact {
                credential_source_id: CredentialSourceId(good),
            },
        )],
        &sources,
        &store,
    )
    .await
    .expect("an Exact binding materializes");

    assert_eq!(
        resolved[0].credential.as_ref().unwrap().expose_secret(),
        "sk-mcp-secret"
    );
}

#[tokio::test]
async fn exact_binding_with_missing_source_fails_closed() {
    let store = InMemorySecretStore::new();
    let sources: HashMap<String, CredentialSource> = HashMap::new();

    let err = resolve_mcp_servers(
        &[
            // The healthy def resolves, but the broken one must fail the whole
            // resolution — never a silently unauthenticated server.
            def("docs", CredentialBinding::None),
            def(
                "jira",
                CredentialBinding::Exact {
                    credential_source_id: CredentialSourceId("ghost".into()),
                },
            ),
        ],
        &sources,
        &store,
    )
    .await
    .expect_err("a dangling Exact binding fails the resolution");
    assert!(matches!(err, ResolveError::SourceMissing(_)));
}

#[tokio::test]
async fn pool_binding_fails_over_to_the_first_healthy_member() {
    let store = InMemorySecretStore::new();
    let repo = InMemoryCredentialRepo::new();
    let good = enter(&store, &repo, "sk-pool-good").await;

    let mut sources: HashMap<String, CredentialSource> = HashMap::new();
    sources.insert(
        good.clone(),
        repo.get(&CredentialSourceId(good.clone())).await.unwrap(),
    );
    // Present but Disabled -> materialize fails, must fail over to the next.
    sources.insert("src_disabled".into(), disabled_source("src_disabled"));

    let pool = CredentialPool {
        id: CredentialPoolId("pool1".into()),
        workspace_id: "ws".into(),
        members: vec![
            CredentialPoolMember {
                credential_source_id: CredentialSourceId("src_disabled".into()),
                ordinal: 0,
                enabled: true,
                selection_weight: 0,
            },
            CredentialPoolMember {
                credential_source_id: CredentialSourceId(good.clone()),
                ordinal: 1,
                enabled: true,
                selection_weight: 0,
            },
        ],
    };
    let ctx = PoolCtx { sources, pool };

    let resolved = resolve_mcp_servers(
        &[def(
            "jira",
            CredentialBinding::OneOfCredentialPool {
                credential_pool_id: CredentialPoolId("pool1".into()),
            },
        )],
        &ctx,
        &store,
    )
    .await
    .expect("pool fails over to the healthy member");

    assert_eq!(
        resolved[0].credential.as_ref().unwrap().expose_secret(),
        "sk-pool-good"
    );
}

#[tokio::test]
async fn pool_with_no_usable_member_fails_closed() {
    let store = InMemorySecretStore::new();
    let mut sources: HashMap<String, CredentialSource> = HashMap::new();
    sources.insert("src_disabled".into(), disabled_source("src_disabled"));

    let pool = CredentialPool {
        id: CredentialPoolId("pool1".into()),
        workspace_id: "ws".into(),
        members: vec![CredentialPoolMember {
            credential_source_id: CredentialSourceId("src_disabled".into()),
            ordinal: 0,
            enabled: true,
            selection_weight: 0,
        }],
    };
    let ctx = PoolCtx { sources, pool };

    let err = resolve_mcp_servers(
        &[def(
            "jira",
            CredentialBinding::OneOfCredentialPool {
                credential_pool_id: CredentialPoolId("pool1".into()),
            },
        )],
        &ctx,
        &store,
    )
    .await
    .expect_err("an exhausted pool fails closed");
    assert!(matches!(err, ResolveError::PoolExhausted(_)));
}
