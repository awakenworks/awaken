//! Actual durable root observations consumed by the existing Rust-to-TLA gate.
use super::*;

pub(super) fn root_observation(
    before: Option<&PersistedSession>,
    after: Option<&PersistedSession>,
    now: u64,
) -> serde_json::Value {
    let state = |root: Option<&PersistedSession>| serde_json::json!({"exists": root.is_some(), "revision": root.map_or(0, |s| s.revision.0), "created": root.and_then(|s| s.created_at_unix_ms), "updated": root.and_then(|s| s.updated_at_unix_ms)});
    serde_json::json!({"before": state(before), "after": state(after), "now": now})
}

pub(super) fn root_step(name: &str, before: &PersistedSession, after: &PersistedSession, now: u64) {
    awaken_reliability_testkit::write_observation_trace(
        name,
        "root",
        serde_json::json!([root_observation(Some(before), Some(after), now)]),
    );
}

use awaken_session_contract::*;
use std::path::{Path, PathBuf};

struct CrashControl<'a> {
    app: &'a SessionApplication,
    mode: &'a str,
    marker: &'a Path,
}
impl CrashControl<'_> {
    async fn pause(&self, boundary: &str) {
        if self.mode == boundary {
            std::fs::write(self.marker, boundary).unwrap();
            std::future::pending::<()>().await;
        }
    }
}

#[async_trait::async_trait]
impl SessionRealizationControl for CrashControl<'_> {
    async fn begin_session_realization(
        &self,
        c: BeginSessionRealization,
    ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure> {
        self.app.begin_session_realization(c).await
    }
    async fn activate_session_realization(
        &self,
        c: ActivateSessionRealization,
    ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure> {
        self.app.activate_session_realization(c).await
    }
    async fn acknowledge_session_realization(
        &self,
        c: AcknowledgeSessionRealization,
    ) -> Result<SessionRealizationDirective, SessionRealizationControlFailure> {
        self.pause("before-ack").await;
        let result = self.app.acknowledge_session_realization(c).await?;
        self.pause("after-ack").await;
        Ok(result)
    }
    async fn fail_session_realization(
        &self,
        c: FailSessionRealization,
    ) -> Result<(), SessionRealizationControlFailure> {
        self.app.fail_session_realization(c).await
    }
}

// External-port fixture: exact-generation publication is a durable file effect.
// This proves the production driver's ordering/recovery under its port contract;
// the existing HTTP MCP recovery E2E separately exercises the real adapter.
struct DurableMcpEffect {
    directory: PathBuf,
}
#[async_trait::async_trait]
impl McpAttachmentRealizer for DurableMcpEffect {
    async fn stage_mcp_attachment(
        &self,
        c: StageMcpAttachment,
    ) -> Result<McpRealizationReceipt, RunError> {
        let stage = self.directory.parent().unwrap().join("stages");
        std::fs::create_dir_all(&stage).unwrap();
        std::fs::write(
            stage.join(format!(
                "{}-{}",
                c.generation.attachment_id.0, c.generation.generation.0
            )),
            b"staged",
        )
        .unwrap();
        NoopMcpRealizer.stage_mcp_attachment(c).await
    }
    async fn publish_mcp_generation(&self, g: McpGenerationRef) -> Result<(), RunError> {
        let key = format!("{}-{}", g.attachment_id.0, g.generation.0);
        std::fs::write(self.directory.join(key), b"published").unwrap();
        Ok(())
    }
    async fn drain_mcp_generation(&self, _: McpGenerationRef) -> Result<(), RunError> {
        Ok(())
    }
}
struct InstallProjection;
#[async_trait::async_trait]
impl SessionProjectionSynchronizer for InstallProjection {
    async fn synchronize_session_projection(
        &self,
        _: &str,
        _: &FrozenSessionProjection,
        _: &SessionRealizationLease,
        _: bool,
    ) -> Result<(), RunError> {
        Ok(())
    }
    async fn invalidate_session_realization_lease(
        &self,
        _: &str,
        _: &SessionRealizationLease,
    ) -> Result<(), RunError> {
        Ok(())
    }
}

#[tokio::test]
async fn publication_ack_process_crashes_recover_through_the_shared_driver() {
    // C1 Stage/Activate/Publish succeeded; C2 SIGKILL before or after ACK CAS;
    // C3 replacement incarnation claims the same durable generation; C4 an old
    // incarnation submits its ACK. P1 before-ACK -> published but unacknowledged;
    // P2 after-ACK -> complete durable receipt; P3 both recover via the real
    // begin/drive/ack chain; P4 stale ACK rejects atomically. External publication
    // can be retried; only the fixture's idempotent effect identity stays unique.
    const MODE: &str = "AWAKEN_PUBLICATION_CRASH_MODE";
    const ROOT: &str = "AWAKEN_PUBLICATION_CRASH_ROOT";
    const ID: &str = "publication-crash";
    let run_driver = |root: PathBuf, mode: String, restarted: bool| async move {
        let repo = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open(
                root.join("session.db").to_str().unwrap(),
            )
            .unwrap(),
        );
        let app = application(
            repo.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        );
        let target = SessionRealizationTarget {
            owner: "worker-a".into(),
            runtime_incarnation: if restarted {
                "worker-a/boot-2"
            } else {
                "worker-a/boot-1"
            }
            .into(),
            lease_expires_at_unix_ms: u64::MAX - 1_024,
            reassign_existing_lease: false,
        };
        let marker = root.join("boundary");
        let control = CrashControl {
            app: &app,
            mode: &mode,
            marker: &marker,
        };
        let directive = control
            .begin_session_realization(BeginSessionRealization {
                expected_resources: None,
                session_id: ID.into(),
                target,
            })
            .await
            .unwrap();
        drive_session_realization(
            ID,
            None,
            &control,
            &InstallProjection,
            &DurableMcpEffect {
                directory: root.join("effects"),
            },
            directive,
        )
        .await
        .unwrap();
        repo.get(ID).await.unwrap()
    };
    if let Ok(mode) = std::env::var(MODE) {
        run_driver(PathBuf::from(std::env::var(ROOT).unwrap()), mode, false).await;
        panic!("crash child unexpectedly returned");
    }
    let mut observations = Vec::new();
    for mode in ["before-ack", "after-ack"] {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        std::fs::create_dir(root.join("effects")).unwrap();
        let repo = Arc::new(
            awaken_session_store::SqliteManagedSessionRepository::open(
                root.join("session.db").to_str().unwrap(),
            )
            .unwrap(),
        );
        let (mut session, lease, generation) = super::realization::publication_fixture(ID);
        session.created_at_unix_ms = Some(100);
        session.updated_at_unix_ms = Some(100);
        session.mcp.desired_names = Some(["browser".into()].into());
        create(repo.as_ref(), session).await;
        let status = awaken_reliability_testkit::CrashProcess::new("tests::observation_refinement::publication_ack_process_crashes_recover_through_the_shared_driver", root.join("boundary"))
            .env(MODE, mode).env(ROOT, root).run().unwrap();
        assert!(!status.success());
        let interrupted = repo.get(ID).await.unwrap();
        assert!(
            root.join("effects/browser-1").is_file(),
            "P1 publication precedes ACK"
        );
        assert_eq!(
            interrupted.mcp.attachments[0].publication_acknowledged,
            mode == "after-ack",
            "P1/P2 durable boundary"
        );
        let recovered = run_driver(root.to_owned(), String::new(), true).await;
        assert_eq!(recovered.created_at_unix_ms, Some(100));
        assert!(recovered.mcp.attachments[0].publication_acknowledged, "P3");
        assert_eq!(
            std::fs::read_dir(root.join("effects")).unwrap().count(),
            1,
            "P3 same external generation identity"
        );
        let app = application(
            repo.clone(),
            Arc::new(RecordingEnvironmentSource::default()),
        );
        let error = app
            .acknowledge_session_realization(AcknowledgeSessionRealization {
                session_id: ID.into(),
                lease,
                published: vec![generation],
                drained: vec![],
            })
            .await
            .unwrap_err();
        assert!(
            matches!(error, SessionRealizationControlFailure::StaleOwnership),
            "P4 {error}"
        );
        assert_eq!(
            repo.get(ID).await.unwrap(),
            recovered,
            "P4 full-root atomic rejection"
        );
        let active = recovered
            .mcp
            .attachments
            .iter()
            .filter(|a| a.state == McpAttachmentState::Active)
            .map(|a| format!("{}-{}", a.attachment_id.0, a.generation.0))
            .collect::<Vec<_>>();
        let published = std::fs::read_dir(root.join("effects"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>();
        let staged = std::fs::read_dir(root.join("stages"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect::<Vec<_>>();
        observations.push(serde_json::json!({"staged": staged, "active": active, "published": published, "drained": []}));
    }
    awaken_reliability_testkit::write_observation_trace(
        "publication-crash-recovery",
        "publication",
        serde_json::json!(observations),
    );
}
