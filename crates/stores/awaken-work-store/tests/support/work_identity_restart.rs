//! Parent-owned native process restart, with exact child-selection assertions.
use super::work_postgres_fixture::WorkPostgresFixture;
use awaken_store_conformance::work_queue::WorkBackend;
use awaken_work_store::{PostgresWorkQueue, SqliteWorkQueue};

async fn phase<Q: WorkBackend>(queue: &Q, phase: &str, record: &str) {
    match phase {
        "seed" => {
            let old = queue
                .enqueue_session("restart", &"old".into())
                .await
                .unwrap();
            queue.remove_env("restart").await.unwrap();
            std::fs::write(record, old).unwrap();
        }
        "assert" => {
            let old = std::fs::read_to_string(record).unwrap();
            let next = queue
                .enqueue_session("restart", &"new".into())
                .await
                .unwrap();
            assert_ne!(next, old, "clear + process restart must not recycle an id");
            let generation = |id: &str| id[6..38].to_owned();
            assert_eq!(
                generation(&old),
                generation(&next),
                "physical store generation must survive restart"
            );
            assert_eq!(&old[38..], "0000000000000000");
            assert_eq!(&next[38..], "0000000000000001");
        }
        _ => panic!("invalid parent-owned restart phase"),
    }
}

#[tokio::test]
#[ignore = "requires exact parent-owned identity fixture"]
async fn identity_restart_child() {
    let kind = std::env::var("AWAKEN_IDENTITY_RESTART_KIND").unwrap();
    let endpoint = std::env::var("AWAKEN_IDENTITY_RESTART_ENDPOINT").unwrap();
    let current = std::env::var("AWAKEN_IDENTITY_RESTART_PHASE").unwrap();
    let record = std::env::var("AWAKEN_IDENTITY_RESTART_RECORD").unwrap();
    match kind.as_str() {
        "sqlite" => {
            phase(
                &SqliteWorkQueue::open(&endpoint, awaken_time::system_clock())
                    .await
                    .unwrap(),
                &current,
                &record,
            )
            .await
        }
        "postgres" => {
            phase(
                &PostgresWorkQueue::connect(&endpoint, awaken_time::system_clock())
                    .await
                    .unwrap(),
                &current,
                &record,
            )
            .await
        }
        _ => panic!("invalid restart kind"),
    }
    eprintln!("identity restart {kind}/{current} storage assertions completed");
}

fn run(kind: &str, endpoint: &str, record: &std::path::Path) {
    for phase in ["seed", "assert"] {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child
            .args([
                "--exact",
                "restart::identity_restart_child",
                "--ignored",
                "--nocapture",
            ])
            .env("AWAKEN_IDENTITY_RESTART_KIND", kind)
            .env("AWAKEN_IDENTITY_RESTART_ENDPOINT", endpoint)
            .env("AWAKEN_IDENTITY_RESTART_PHASE", phase)
            .env("AWAKEN_IDENTITY_RESTART_RECORD", record);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            child.creation_flags(0x08000000);
        }
        let output = child.output().unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "{kind}/{phase}: {stdout}\n{stderr}"
        );
        assert!(
            stdout.contains("test restart::identity_restart_child ... ok"),
            "{stdout}"
        );
        assert!(
            stdout.contains("test result: ok. 1 passed; 0 failed; 0 ignored;"),
            "{stdout}"
        );
        assert!(
            stderr.contains(&format!(
                "identity restart {kind}/{phase} storage assertions completed"
            )),
            "{stderr}"
        );
    }
}

#[test]
fn sqlite_identity_high_watermark_survives_a_native_process_restart() {
    let directory = tempfile::tempdir().unwrap();
    run(
        "sqlite",
        directory.path().join("restart.db").to_str().unwrap(),
        &directory.path().join("retired-id.txt"),
    );
}

#[tokio::test]
async fn postgres_identity_high_watermark_survives_a_native_process_restart() {
    let url = awaken_reliability_testkit::postgres_fixture_url()
        .expect("native PostgreSQL restart requires fixture");
    let fixture = WorkPostgresFixture::new(&url, 2).await;
    let endpoint = fixture.scoped_url.clone();
    let directory = tempfile::tempdir().unwrap();
    let record = directory.path().join("retired-id.txt");
    tokio::task::spawn_blocking(move || run("postgres", &endpoint, &record))
        .await
        .unwrap();
    fixture.cleanup().await;
}
