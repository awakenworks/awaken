//! Conformance suite for [`CredentialRepo`]: the same behavioral assertions run
//! against the in-memory repo and (feature `sqlite`) the sqlite repo, so both
//! backends keep identical semantics — workspace-scoped lists, upsert puts, and
//! the exact NotFound arms.

use awaken_credential_vault::repo::{CredentialRepo, InMemoryCredentialRepo};
use awaken_credential_vault::{
    CredentialError, CredentialKind, CredentialPool, CredentialPoolId, CredentialPoolMember,
    CredentialSource, CredentialSourceId, CredentialStatus,
};

fn source(id: &str, ws: &str) -> CredentialSource {
    CredentialSource {
        id: CredentialSourceId(id.into()),
        workspace_id: ws.into(),
        kind: CredentialKind::Vault,
        provider_id: Some("anthropic".into()),
        env_key: Some("ANTHROPIC_API_KEY".into()),
        material_ref: None,
        status: CredentialStatus::Active,
        version: 1,
    }
}

fn pool(id: &str, ws: &str) -> CredentialPool {
    CredentialPool {
        id: CredentialPoolId(id.into()),
        workspace_id: ws.into(),
        members: vec![CredentialPoolMember {
            credential_source_id: CredentialSourceId("cred:a".into()),
            ordinal: 0,
            enabled: true,
            selection_weight: 0,
        }],
    }
}

async fn sources_round_trip_and_scope_by_workspace(repo: &dyn CredentialRepo) {
    repo.put(source("cred:a", "ws")).await.unwrap();
    repo.put(source("cred:b", "ws")).await.unwrap();
    repo.put(source("cred:c", "other")).await.unwrap();

    let got = repo
        .get(&CredentialSourceId("cred:a".into()))
        .await
        .unwrap();
    assert_eq!(got, source("cred:a", "ws"));
    assert_eq!(repo.list("ws").await.unwrap().len(), 2);
    assert_eq!(repo.list("other").await.unwrap().len(), 1);
    assert_eq!(repo.list("empty").await.unwrap().len(), 0);
}

async fn pools_round_trip_and_scope_by_workspace(repo: &dyn CredentialRepo) {
    repo.put_pool(pool("pool:a", "ws")).await.unwrap();
    repo.put_pool(pool("pool:b", "ws")).await.unwrap();
    repo.put_pool(pool("pool:c", "other")).await.unwrap();

    let got = repo
        .get_pool(&CredentialPoolId("pool:a".into()))
        .await
        .unwrap();
    assert_eq!(got, pool("pool:a", "ws"));
    assert_eq!(repo.list_pools("ws").await.unwrap().len(), 2);
    assert_eq!(repo.list_pools("other").await.unwrap().len(), 1);
    assert_eq!(repo.list_pools("empty").await.unwrap().len(), 0);
}

async fn missing_rows_are_not_found(repo: &dyn CredentialRepo) {
    assert!(matches!(
        repo.get(&CredentialSourceId("cred:absent".into())).await,
        Err(CredentialError::SourceNotFound(id)) if id == "cred:absent"
    ));
    assert!(matches!(
        repo.get_pool(&CredentialPoolId("pool:absent".into())).await,
        Err(CredentialError::PoolNotFound(id)) if id == "pool:absent"
    ));
}

async fn put_is_upsert(repo: &dyn CredentialRepo) {
    repo.put(source("cred:a", "ws")).await.unwrap();
    let mut v2 = source("cred:a", "ws");
    v2.status = CredentialStatus::Disabled;
    v2.version = 2;
    repo.put(v2.clone()).await.unwrap();
    let got = repo
        .get(&CredentialSourceId("cred:a".into()))
        .await
        .unwrap();
    assert_eq!(got, v2);
    assert_eq!(repo.list("ws").await.unwrap().len(), 1);

    repo.put_pool(pool("pool:a", "ws")).await.unwrap();
    let mut p2 = pool("pool:a", "ws");
    p2.members.clear();
    repo.put_pool(p2.clone()).await.unwrap();
    let got = repo
        .get_pool(&CredentialPoolId("pool:a".into()))
        .await
        .unwrap();
    assert_eq!(got, p2);
    assert_eq!(repo.list_pools("ws").await.unwrap().len(), 1);
}

/// Run every suite, each on a fresh repo from `make`.
async fn run_all(make: impl Fn() -> Box<dyn CredentialRepo>) {
    sources_round_trip_and_scope_by_workspace(&*make()).await;
    pools_round_trip_and_scope_by_workspace(&*make()).await;
    missing_rows_are_not_found(&*make()).await;
    put_is_upsert(&*make()).await;
}

#[tokio::test]
async fn in_memory_repo_conforms() {
    run_all(|| Box::new(InMemoryCredentialRepo::new())).await;
}

#[cfg(feature = "sqlite")]
#[tokio::test]
async fn sqlite_repo_conforms() {
    use awaken_credential_vault::sqlite::SqliteCredentialRepo;
    run_all(|| Box::new(SqliteCredentialRepo::open_in_memory().unwrap())).await;
}
