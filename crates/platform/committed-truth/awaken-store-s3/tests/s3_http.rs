mod support;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::thread::read::transcript::{TranscriptPageRequest, TranscriptView};
use awaken_lease_testkit::ManualClock;
use awaken_store_s3::{Limits, S3Storage, SessionKey};
use bytes::Bytes;
use futures::TryStreamExt;
use object_store::{ObjectStore, ObjectStoreExt, PutMode, PutOptions, UpdateVersion, path::Path};
use std::sync::Arc;

/// An independently configured S3 endpoint is required; absence never counts as a pass.
#[tokio::test]
#[expect(
    clippy::disallowed_methods,
    reason = "this test composition root selects its explicit S3 fixture"
)]
#[ignore = "requires AWAKEN_TEST_S3_ENDPOINT, AWAKEN_TEST_S3_BUCKET and AWS credentials"]
async fn conditional_s3_publication_and_cold_range_replay() {
    let endpoint = std::env::var("AWAKEN_TEST_S3_ENDPOINT").expect("explicit S3 endpoint");
    let bucket = std::env::var("AWAKEN_TEST_S3_BUCKET").expect("task-owned test bucket");
    let connect = || {
        awaken_object_storage::s3_transport(
            &bucket,
            Some("us-east-1"),
            Some(&endpoint),
            endpoint.starts_with("http://"),
        )
        .unwrap()
    };
    let store: Arc<dyn ObjectStore> = connect();
    let prefix = format!("awaken-conformance-{}", std::process::id());
    let probe = Path::from(format!("{prefix}/probe"));
    let first = store
        .put_opts(
            &probe,
            Bytes::from_static(b"abcdef").into(),
            PutOptions {
                mode: PutMode::Create,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    // A server can close the connection on a rejected conditional PUT.
    // Probe those responses with independent clients and check the error class.
    let duplicate = connect()
        .put_opts(
            &probe,
            Bytes::from_static(b"duplicate").into(),
            PutOptions {
                mode: PutMode::Create,
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(
        duplicate,
        object_store::Error::AlreadyExists { .. } | object_store::Error::Precondition { .. }
    ));
    let old = UpdateVersion {
        e_tag: first.e_tag,
        version: first.version,
    };
    store
        .put_opts(
            &probe,
            Bytes::from_static(b"ghijkl").into(),
            PutOptions {
                mode: PutMode::Update(old.clone()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let stale = connect()
        .put_opts(
            &probe,
            Bytes::from_static(b"stale").into(),
            PutOptions {
                mode: PutMode::Update(old),
                ..Default::default()
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(stale, object_store::Error::Precondition { .. }));
    assert_eq!(
        store.get_range(&probe, 1..4).await.unwrap(),
        Bytes::from_static(b"hij")
    );
    let clock = Arc::new(ManualClock::new(1_768_000_000_000));
    let engine = S3Storage::new(store.clone(), &prefix, clock.clone(), Limits::default()).unwrap();
    let session = engine.session(SessionKey::new("tenant", "session").unwrap());
    session.create().await.unwrap();
    let claim = support::activate(&session, "run", "session")
        .await
        .run_claim();
    for n in 0..4 {
        session
            .commit(
                &claim,
                support::operation("run", "session", n, n, &format!("frame-{n}")),
            )
            .await
            .unwrap();
    }
    session.pack().await.unwrap();
    let cold = S3Storage::new(store.clone(), &prefix, clock, Limits::default())
        .unwrap()
        .session(session.key().clone());
    let page = cold
        .read_page(TranscriptPageRequest {
            thread_id: ThreadId("session".into()),
            view: TranscriptView::RawCommitted,
            snapshot: None,
            start: 1,
            limit: std::num::NonZeroUsize::new(2).unwrap(),
        })
        .await
        .unwrap();
    assert_eq!(
        page.messages
            .iter()
            .map(|m| m.text_content())
            .collect::<Vec<_>>(),
        vec!["frame-1", "frame-2"]
    );
    cold.collect_garbage().await.unwrap();
    // Remove only objects under the task's exact test prefix.
    let objects = store
        .list(Some(&Path::from(format!("{prefix}/"))))
        .try_collect::<Vec<_>>()
        .await
        .unwrap();
    for object in objects {
        assert!(object.location.as_ref().starts_with(&format!("{prefix}/")));
        store.delete(&object.location).await.unwrap();
    }
}
