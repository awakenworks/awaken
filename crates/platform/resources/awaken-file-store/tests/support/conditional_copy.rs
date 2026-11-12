//! Conditional object-copy commit conformance and fault injection.

use super::tests::object_store_conformance;
use super::*;
use object_store::memory::InMemory;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CommitFailure {
    UploadBefore,
    UploadAfter,
    CopyBefore,
    CopyAfter,
    Cleanup,
}

#[derive(Debug)]
struct FailingCommitStore {
    inner: InMemory,
    failure: CommitFailure,
}

impl std::fmt::Display for FailingCommitStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("failing commit fixture")
    }
}

fn injected_failure() -> object_store::Error {
    object_store::Error::Generic {
        store: "commit fixture",
        source: std::io::Error::other("injected failure").into(),
    }
}

#[async_trait]
impl ObjectStore for FailingCommitStore {
    async fn put_opts(
        &self,
        path: &ObjPath,
        payload: PutPayload,
        options: PutOptions,
    ) -> object_store::Result<object_store::PutResult> {
        if self.failure == CommitFailure::UploadBefore {
            return Err(injected_failure());
        }
        let result = self.inner.put_opts(path, payload, options).await?;
        if self.failure == CommitFailure::UploadAfter {
            return Err(injected_failure());
        }
        Ok(result)
    }
    async fn put_multipart_opts(
        &self,
        path: &ObjPath,
        options: object_store::PutMultipartOptions,
    ) -> object_store::Result<Box<dyn object_store::MultipartUpload>> {
        self.inner.put_multipart_opts(path, options).await
    }
    async fn get_opts(
        &self,
        path: &ObjPath,
        options: object_store::GetOptions,
    ) -> object_store::Result<object_store::GetResult> {
        self.inner.get_opts(path, options).await
    }
    fn delete_stream(
        &self,
        locations: futures::stream::BoxStream<'static, object_store::Result<ObjPath>>,
    ) -> futures::stream::BoxStream<'static, object_store::Result<ObjPath>> {
        if self.failure == CommitFailure::Cleanup {
            return locations.map(|_| Err(injected_failure())).boxed();
        }
        self.inner.delete_stream(locations)
    }
    fn list(
        &self,
        prefix: Option<&ObjPath>,
    ) -> futures::stream::BoxStream<'static, object_store::Result<object_store::ObjectMeta>> {
        self.inner.list(prefix)
    }
    async fn list_with_delimiter(
        &self,
        prefix: Option<&ObjPath>,
    ) -> object_store::Result<object_store::ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }
    async fn copy_opts(
        &self,
        from: &ObjPath,
        to: &ObjPath,
        options: object_store::CopyOptions,
    ) -> object_store::Result<()> {
        if self.failure == CommitFailure::CopyBefore {
            return Err(injected_failure());
        }
        self.inner.copy_opts(from, to, options).await?;
        if self.failure == CommitFailure::CopyAfter {
            return Err(injected_failure());
        }
        Ok(())
    }
}

#[tokio::test]
async fn copy_commit_failures_reclaim_only_staging_and_preserve_ambiguous_commits() {
    // Cause/effect rules: R1 upload fails before/after its external effect ->
    // no committed key, staging reclaimed. R2 copy fails before effect -> same.
    // R3 copy commits then transport fails -> retain immutable final bytes,
    // reclaim staging, return Indeterminate. R4 cleanup fails -> return
    // Indeterminate, retain final plus invisible staging; never delete final.
    for failure in [
        CommitFailure::UploadBefore,
        CommitFailure::UploadAfter,
        CommitFailure::CopyBefore,
        CommitFailure::CopyAfter,
        CommitFailure::Cleanup,
    ] {
        let backend = Arc::new(FailingCommitStore {
            inner: InMemory::new(),
            failure,
        });
        let mut store = ObjectFileStore::new(backend.clone(), "failure");
        store.conditional_copy = true;
        assert!(
            matches!(
                store.put(b"immutable").await,
                Err(FileStoreError::Indeterminate { .. })
            ),
            "{failure:?}"
        );
        let committed = matches!(failure, CommitFailure::CopyAfter | CommitFailure::Cleanup);
        assert_eq!(
            store
                .get(&content_id(b"immutable"))
                .await
                .unwrap()
                .is_some(),
            committed,
            "{failure:?}"
        );
        let raw = backend.inner.list(None).collect::<Vec<_>>().await;
        assert_eq!(
            raw.len(),
            usize::from(committed) + usize::from(failure == CommitFailure::Cleanup),
            "{failure:?}"
        );
        assert_eq!(
            store.list().await.unwrap().len(),
            usize::from(committed),
            "temporary keys are never Files"
        );
    }
}

#[tokio::test]
async fn conditional_copy_preserves_the_same_immutable_store_contract()
-> Result<(), Box<dyn std::error::Error>> {
    // R1 copy-commit uses the same content-id/replay/corruption/list/delete
    // cause-effect table above. R2 concurrent identical writers -> one
    // committed key and no temporary objects after every writer completes.
    object_store_conformance(Arc::new(InMemory::new()), "copied", true).await?;
    let backend = Arc::new(InMemory::new());
    let mut store = ObjectFileStore::new(backend.clone(), "concurrent");
    store.conditional_copy = true;
    let (a, b, c) = tokio::join!(store.put(b"same"), store.put(b"same"), store.put(b"same"));
    assert_eq!(a?, b?);
    assert_eq!(store.list().await?, vec![c?]);
    assert_eq!(
        backend.list(None).collect::<Vec<_>>().await.len(),
        1,
        "R2 staging reclaimed"
    );
    Ok(())
}

#[test]
fn disabled_conditional_put_requires_one_configured_atomic_copy_commit() {
    // Decision table: R1 default/etag -> conditional PUT, even if a copy
    // capability exists. R2 disabled PUT + configured copy -> copy commit.
    // R3 disabled PUT + absent/blank copy -> reject before any network write.
    for put in [None, Some("etag")] {
        assert!(!conditional_copy_configuration(put, None).unwrap(), "R1");
        assert!(
            !conditional_copy_configuration(put, Some("header:key:value")).unwrap(),
            "R1"
        );
    }
    assert!(
        conditional_copy_configuration(Some("disabled"), Some("header:key:value")).unwrap(),
        "R2"
    );
    for copy in [None, Some(""), Some(" ")] {
        assert!(
            conditional_copy_configuration(Some("disabled"), copy).is_err(),
            "R3"
        );
    }
}

#[test]
fn oss_provider_capabilities_apply_only_to_exact_official_https_s3_endpoints() {
    // R1 official public/internal S3 OSS host -> bucket-scoped virtual host;
    // R2 ordinary/custom/S3 lookalike, HTTP, path or suffix-spoofed host ->
    // no OSS capability inference. Existing allocation validation remains
    // the authority for the bucket and prefix, not this endpoint classifier.
    for region in ["cn-hangzhou", "cn-hangzhou-internal"] {
        assert_eq!(
            oss_endpoint_with_bucket(&format!("https://s3.oss-{region}.aliyuncs.com"), "content"),
            Some(format!("https://content.s3.oss-{region}.aliyuncs.com")),
            "R1"
        );
    }
    for endpoint in [
        "http://s3.oss-cn-hangzhou.aliyuncs.com",
        "http://minio:80",
        "https://s3.oss-cn-hangzhou.aliyuncs.com.attacker.test",
        "https://s3.oss-a.b.aliyuncs.com",
        "https://s3.oss-cn-hangzhou.aliyuncs.com/path",
    ] {
        assert!(
            oss_endpoint_with_bucket(endpoint, "content").is_none(),
            "R2 {endpoint}"
        );
    }
}
