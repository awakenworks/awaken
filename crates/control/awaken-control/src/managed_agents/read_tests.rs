// @formal surface exclude=test_only
// @formal owner crates/control/awaken-control/src/managed_agents.rs
// @formal reason: Regression for exact revision visibility and bounded publication reads.
use super::test_support::{create_params, plane_with_catalog, plane_with_store, update_params};
use super::*;
use awaken_agent_config::{
    ConfigStoreError, PublicationWrite, ScopedConfigRegistry, StoredPublication,
};
use awaken_config_store::SqliteConfigStore;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};

struct PublicationReadProbe {
    inner: Arc<SqliteConfigStore>,
    reads: Mutex<BTreeMap<String, usize>>,
    fail_publications: AtomicBool,
}

impl PublicationReadProbe {
    fn take_reads(&self) -> BTreeMap<String, usize> {
        std::mem::take(&mut *self.reads.lock().unwrap())
    }
}

#[async_trait::async_trait]
impl ScopedConfigRegistry for PublicationReadProbe {
    async fn list_config_scopes(&self) -> Result<Vec<ScopeId>, ConfigStoreError> {
        self.inner.list_config_scopes().await
    }
    async fn put_config_scoped(
        &self,
        scope: &ScopeId,
        config: &AgentConfig,
    ) -> Result<(), ConfigStoreError> {
        self.inner.put_config_scoped(scope, config).await
    }
    async fn get_config_scoped(
        &self,
        scope: &ScopeId,
        id: &str,
    ) -> Result<Option<AgentConfig>, ConfigStoreError> {
        self.inner.get_config_scoped(scope, id).await
    }
    async fn get_config_revision_scoped(
        &self,
        scope: &ScopeId,
        id: &str,
    ) -> Result<Option<AgentConfigRevision>, ConfigStoreError> {
        self.inner.get_config_revision_scoped(scope, id).await
    }
    async fn list_config_revisions_scoped(
        &self,
        scope: &ScopeId,
        id: &str,
    ) -> Result<Vec<AgentConfigRevision>, ConfigStoreError> {
        self.inner.list_config_revisions_scoped(scope, id).await
    }
    async fn list_configs_scoped(
        &self,
        scope: &ScopeId,
    ) -> Result<Vec<AgentConfig>, ConfigStoreError> {
        self.inner.list_configs_scoped(scope).await
    }
    async fn put_publication_if_source_current_scoped(
        &self,
        scope: &ScopeId,
        publication: &StoredPublication,
    ) -> Result<PublicationWrite, ConfigStoreError> {
        self.inner
            .put_publication_if_source_current_scoped(scope, publication)
            .await
    }
    async fn get_publication_scoped(
        &self,
        scope: &ScopeId,
        fingerprint: &str,
    ) -> Result<Option<StoredPublication>, ConfigStoreError> {
        self.inner.get_publication_scoped(scope, fingerprint).await
    }
    async fn list_published_scoped(
        &self,
        scope: &ScopeId,
    ) -> Result<Vec<StoredPublication>, ConfigStoreError> {
        *self
            .reads
            .lock()
            .unwrap()
            .entry(scope.0.clone())
            .or_default() += 1;
        if self.fail_publications.load(Ordering::SeqCst) {
            return Err(ConfigStoreError("publication read unavailable".into()));
        }
        self.inner.list_published_scoped(scope).await
    }
}

#[tokio::test]
async fn immutable_agent_reads_do_not_repeat_published_table_loads() {
    // Cause/effect graph: growing immutable history H + requested revision V +
    // durable publication P + request scope S + store availability A determine
    // exact returned version, draft/tenant visibility, bounded reads, and error.
    // Rules (each operation starts a fresh request; no cross-request cache):
    // R1 H=2,V=1,P=yes,S=owner,A=yes => exact r1; one owner read, no unused
    // reserved read. R2 H=2,V=all/current/list,P=yes,S=owner => r1..r2/r2,
    // <=1 read per needed scope; the unpublished draft needs reserved fallback.
    // R3 V=3,P=no => NotFound; saving a draft cannot expose or hide r2.
    // R4 S=foreign => NotFound; no owner or reserved-scope read.
    // R5 A=no => Storage error; never an empty/successful response.
    // The probe wraps the real SQLite store. Publication-table read count is a
    // deterministic workload bound; elapsed-time assertions would be flaky and
    // could pass the 204-version incident on an unusually fast machine.
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("config.sqlite");
    let path = path.to_str().unwrap();
    let (authored, _) = plane_with_catalog(path).await;
    let writer = ConfigPlaneManagedAgentRepository::new(
        authored.clone(),
        "workspace-a",
        awaken_time::system_clock(),
    );
    let mut agent = writer
        .create("workspace-a", create_params("revision 1"))
        .await
        .unwrap();
    for revision in 2..=2 {
        let mut params = update_params(agent.version);
        params.name = Some(format!("revision {revision}"));
        agent = writer
            .update("workspace-a", &agent.id, params)
            .await
            .unwrap();
    }
    let scope = ScopeId::from("workspace-a");
    let mut draft = authored
        .get_versioned(&scope, &agent.id)
        .await
        .unwrap()
        .unwrap()
        .config;
    draft.name = Some("unpublished revision 3".into());
    authored.put_if_revision(&scope, &draft, 2).await.unwrap();
    let probe = Arc::new(PublicationReadProbe {
        inner: Arc::new(SqliteConfigStore::open(path).await.unwrap()),
        reads: Mutex::new(BTreeMap::new()),
        fail_publications: AtomicBool::new(false),
    });
    let (read_plane, _) = plane_with_store(probe.clone());
    let reader = ConfigPlaneManagedAgentRepository::new(
        read_plane,
        "workspace-a",
        awaken_time::system_clock(),
    );
    let expected_reads =
        BTreeMap::from([("workspace-a".into(), 1), (RESERVED_ADMIN_SCOPE.into(), 1)]);
    let exact = reader
        .retrieve("workspace-a", &agent.id, Some(1))
        .await
        .unwrap();
    assert_eq!(
        (exact.version, exact.name.as_str()),
        (1, "revision 1"),
        "R1 exact immutable version"
    );
    assert_eq!(
        probe.take_reads(),
        BTreeMap::from([("workspace-a".into(), 1)]),
        "R1 bounded publication reads"
    );
    let versions = reader.versions("workspace-a", &agent.id).await.unwrap();
    assert_eq!(
        versions.iter().map(|v| v.version).collect::<Vec<_>>(),
        vec![1, 2],
        "R2 history excludes draft"
    );
    assert_eq!(probe.take_reads(), expected_reads, "R2 version list bound");
    assert_eq!(
        reader
            .retrieve("workspace-a", &agent.id, None)
            .await
            .unwrap()
            .version,
        2,
        "R2 current remains published r2"
    );
    assert_eq!(probe.take_reads(), expected_reads, "R2 current bound");
    let listed = reader
        .list("workspace-a", &AgentListParams::default())
        .await
        .unwrap();
    assert_eq!(
        listed.iter().map(|agent| agent.version).collect::<Vec<_>>(),
        vec![2],
        "R2 list excludes draft"
    );
    assert_eq!(probe.take_reads(), expected_reads, "R2 list bound");
    assert!(
        matches!(
            reader.retrieve("workspace-a", &agent.id, Some(3)).await,
            Err(ManagedAgentError::NotFound)
        ),
        "R3 exact draft remains hidden"
    );
    assert_eq!(
        probe.take_reads(),
        expected_reads,
        "R3 missing revision bound"
    );
    assert!(
        matches!(
            reader.retrieve("workspace-b", &agent.id, Some(1)).await,
            Err(ManagedAgentError::NotFound)
        ),
        "R4 foreign scope"
    );
    assert!(
        probe.take_reads().is_empty(),
        "R4 no publication read for absent scoped history"
    );
    probe.fail_publications.store(true, Ordering::SeqCst);
    assert!(
        matches!(
            reader.retrieve("workspace-a", &agent.id, Some(1)).await,
            Err(ManagedAgentError::Storage(_))
        ),
        "R5 fail closed"
    );
}
