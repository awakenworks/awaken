// @formal surface requirement=session.multiagent.thread_authority
// @formal reason: The Coordinator's Session execution for Sessions its registered Workers execute: it lowers
// @formal reason: each admitted command over the installed frozen projection into durable dispatch rows and
// @formal reason: never executes, stages or realizes anything itself. Classification, not a formal-proof
// @formal reason: claim.
//! [`SessionRunDelivery`]: the Coordinator's Session execution for the
//! Sessions its registered Workers execute.
//!
//! The Session application admits each command; this lowers it over the
//! Session's installed frozen projection ([`SessionProjections`]) into the
//! durable dispatch rows a registered Worker claims. It verifies the live
//! Resource bindings the projection freezes and reads the pinned Skill bundles
//! slash commands expand from, each against its digest; it stages, mounts and
//! realizes nothing.

use std::sync::Arc;

use awaken_dispatch_erasure::AnyCoordinatorDispatch;
use awaken_dispatch_execution::session_dispatch::DispatchPreparation;
use awaken_resource_contract::{LiveResourceBindingVerifier, RepositoryBindingVerifier};
use awaken_session_configuration_contract::SkillBundleSource;
use awaken_session_configuration_contract::resource::binding_checks::verify_manifest_bindings;
use awaken_session_configuration_contract::skill_delivery::{
    load_pinned_skills, skill_store_run_error,
};
use awaken_session_execution_contract::{AdmitSessionRun, SessionRunReservation};
use awaken_session_realization_contract::ResourceOperationFence;
use awaken_session_types::RunError;

use crate::session_run_reservation::{
    SessionRunInputs, reserve_session_run, validate_session_run_command,
};
use crate::{
    DispatchProjection, RUN_PUBLICATIONS, SessionProjections, managed_skill_registry,
    session_activation_snapshot,
};

/// The live authorities a Session's frozen Resource bindings are verified
/// against before a Run is admitted.
#[derive(Clone, Default)]
pub struct ResourceBindingVerifiers {
    pub memory: Option<Arc<dyn LiveResourceBindingVerifier>>,
    pub repositories: Option<Arc<dyn RepositoryBindingVerifier<ResourceOperationFence>>>,
}

/// The Coordinator's Session execution over registered Workers. Consumers:
/// the Session application, through its Worker-placement roles.
pub struct SessionRunDelivery {
    projections: Arc<SessionProjections>,
    dispatch: Arc<AnyCoordinatorDispatch>,
    skill_bundles: Option<Arc<dyn SkillBundleSource<ResourceOperationFence>>>,
    resource_bindings: ResourceBindingVerifiers,
}

impl SessionRunDelivery {
    /// Delivery of the Sessions `projections` holds into `dispatch`, reading
    /// pinned Skill bundles from `skill_bundles`.
    #[must_use]
    pub fn new(
        projections: Arc<SessionProjections>,
        dispatch: Arc<AnyCoordinatorDispatch>,
        skill_bundles: Option<Arc<dyn SkillBundleSource<ResourceOperationFence>>>,
        resource_bindings: ResourceBindingVerifiers,
    ) -> Self {
        Self {
            projections,
            dispatch,
            skill_bundles,
            resource_bindings,
        }
    }

    /// The frozen projections Runs are admitted over.
    #[must_use]
    pub fn projections(&self) -> &Arc<SessionProjections> {
        &self.projections
    }

    /// The installed projection of `session`; a Run of a Session whose
    /// projection is not installed yet is refused as retryable.
    fn installed(&self, session: &str) -> Result<Arc<DispatchProjection>, RunError> {
        self.projections.projection(session)?.ok_or_else(|| {
            RunError::unavailable_classified(
                "session_projection_not_installed",
                format!("Session {session} has no installed frozen projection on this Coordinator"),
            )
        })
    }

    fn preparation(
        &self,
        activation: awaken_runtime_contract::RunActivation,
    ) -> DispatchPreparation<'_> {
        DispatchPreparation::new(
            activation,
            Box::new(self.projections.as_ref()),
            self.projections.local_workspace(),
            self.projections.publications(),
            RUN_PUBLICATIONS,
            false,
        )
    }

    /// Reserve the Session application's admitted Run over the Session's
    /// installed projection.
    ///
    /// Decision table (causes → effect; the lowering's own rules are
    /// [`reserve_session_run`]'s):
    /// | Rule | cause | effect |
    /// |---|---|---|
    /// | C1 | no projection is installed | retryable refusal; nothing is read or reserved |
    /// | C2 | a frozen MemoryStore or Repository binding changed | refused before the Skill store is read |
    /// | C3 | the projection pins no Agent publication | refused |
    /// | C4 | a pinned Skill bundle is missing or mismatched | refused; nothing is reserved |
    /// | C5 | every input holds | one unclaimable row placed on a registered Worker, carrying the desired manifest and the Session's frozen model |
    pub async fn reserve_session_run(
        &self,
        command: AdmitSessionRun,
    ) -> Result<SessionRunReservation, RunError> {
        validate_session_run_command(&command)?;
        let projection = self.installed(command.session_id.as_str())?;
        verify_manifest_bindings(
            projection.manifest(),
            self.resource_bindings.memory.as_deref(),
            self.resource_bindings.repositories.as_deref(),
        )
        .await?;
        let publication = projection.publication().cloned().ok_or_else(|| {
            RunError::bad_request("a Worker-placed Session requires a pinned Agent publication")
        })?;
        let versions = load_pinned_skills(
            self.skill_bundles.as_deref(),
            projection.workspace(),
            projection.manifest().resources.skills(),
            |_| Ok(None),
        )
        .await
        .map_err(skill_store_run_error)?;
        let skills =
            managed_skill_registry(versions, Some(&publication), Some(projection.tools()))?;
        let snapshot = session_activation_snapshot(publication, Some(projection.tools()), true)?;
        reserve_session_run(
            command,
            SessionRunInputs {
                snapshot: &snapshot,
                skills: skills.as_deref(),
                model_ref_override: Some(projection.baseline().execution_model_ref.clone()),
            },
            |activation| self.preparation(activation),
            self.dispatch.as_ref(),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{projection, publication};
    use awaken_agent_contract::AgentSkillKind;
    use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
    use awaken_agent_contract::agent::run::Id as RunId;
    use awaken_dispatch_contract::{SessionRunAdmissionObservation, SessionRunAdmissionReads as _};
    use awaken_resource_contract::{
        ConfigVersion, MemoryStoreConfigVersion, ResourceRegistryError, RetentionPolicy,
        SkillBundleFile, SkillVersion, skill_bundle_sha256,
    };
    use awaken_session_configuration_contract::SkillBundleSourceError;
    use awaken_session_configuration_contract::resource::{
        ResolvedInput, ResolvedInputSource, ResolvedSessionResources, ResolvedSkillBinding,
    };
    use awaken_session_execution_contract::SessionRunExecutionRequirements;

    fn skill() -> SkillVersion {
        let files = vec![SkillBundleFile {
            path: "SKILL.md".into(),
            content:
                b"---\nname: review\ndescription: review code\n---\nReview $ARGUMENTS carefully."
                    .to_vec(),
            executable: false,
        }];
        SkillVersion {
            id: "review-1".into(),
            skill_id: "review".into(),
            version: 1,
            name: "review".into(),
            description: "review code".into(),
            directory: "/skills/review".into(),
            bundle_sha256: skill_bundle_sha256(&files),
            files,
            created_unix_nanos: 0,
        }
    }

    fn pin(version: &SkillVersion) -> ResolvedSkillBinding {
        ResolvedSkillBinding {
            kind: AgentSkillKind::Custom,
            skill_id: version.skill_id.to_string(),
            version: version.version,
            bundle_sha256: version.bundle_sha256.clone(),
        }
    }

    /// The content-addressed store: one stored bundle.
    struct Store(SkillVersion);

    #[async_trait::async_trait]
    impl SkillBundleSource<ResourceOperationFence> for Store {
        async fn load(
            &self,
            _workspace_id: &str,
            binding: &ResolvedSkillBinding,
            _fence: Option<&ResourceOperationFence>,
        ) -> Result<Option<SkillVersion>, SkillBundleSourceError> {
            Ok((binding.skill_id == self.0.skill_id.as_str()).then(|| self.0.clone()))
        }
    }

    /// A live MemoryStore binding at version 4.
    struct Memory;

    #[async_trait::async_trait]
    impl LiveResourceBindingVerifier for Memory {
        async fn verify_memory_binding(
            &self,
            _workspace_id: &str,
            id: &str,
            version: ConfigVersion,
        ) -> Result<(), ResourceRegistryError> {
            (version == ConfigVersion(4))
                .then_some(())
                .ok_or_else(|| ResourceRegistryError::NotFound(id.into()))
        }

        async fn verify_repository_binding(
            &self,
            _workspace_id: &str,
            _id: &str,
            _version: ConfigVersion,
        ) -> Result<(), ResourceRegistryError> {
            unreachable!("no Repository is bound")
        }
    }

    fn memory_input(version: u64) -> ResolvedInput {
        ResolvedInput {
            created_at_unix_ms: None,
            updated_at_unix_ms: None,
            binding_id: "memory".into(),
            source: ResolvedInputSource::MemoryStore {
                memory_store_id: "memory-1".into(),
                config: MemoryStoreConfigVersion {
                    memory_store_id: "memory-1".into(),
                    version: ConfigVersion(version),
                    retention_policy: RetentionPolicy::default(),
                },
            },
            mount_path: "/mnt/memory".into(),
            access: awaken_resource_contract::ResourceAccess::ReadOnly,
            instructions: None,
        }
    }

    fn delivery(stored: Option<SkillVersion>) -> SessionRunDelivery {
        SessionRunDelivery::new(
            Arc::new(SessionProjections::new("local", None)),
            Arc::new(AnyCoordinatorDispatch::from_durable_store(
                awaken_dispatch::MemoryDispatchStore::new(awaken_time::system_clock()),
                None,
            )),
            stored.map(|version| {
                Arc::new(Store(version)) as Arc<dyn SkillBundleSource<ResourceOperationFence>>
            }),
            ResourceBindingVerifiers {
                memory: Some(Arc::new(Memory)),
                repositories: None,
            },
        )
    }

    fn command(session: &str, text: &str) -> AdmitSessionRun {
        AdmitSessionRun {
            session_id: session.into(),
            agent_id: "agent".into(),
            operation_id: "operation-1".into(),
            run_id: RunId(format!("{session}-run")),
            messages: vec![Message::text(
                MessageId("message-1".into()),
                Role::User,
                text,
            )],
            data_subject_id: None,
            traceparent: None,
            execution_requirements: SessionRunExecutionRequirements {
                tool_capability_narrowing:
                    awaken_runtime_contract::permission::ToolCapabilityNarrowing::Configured,
                required_worker_capabilities: Default::default(),
            },
            replacement: awaken_session_types::SessionRunReplacement::PreservePrior,
        }
    }

    /// An Agent that selects the `review` Skill and can read files.
    fn reviewing_agent() -> awaken_runtime_contract::ExecutableAgentSnapshot {
        let mut agent = publication("agent");
        agent.resolved_spec.plugin_config.agent.skills =
            vec![awaken_agent_contract::AgentSkillBinding::custom("review")];
        agent.resolved_spec.tool_descriptors =
            vec![awaken_runtime_contract::resolved::ToolDescriptor::pinned(
                "test",
                "read",
                "read",
                serde_json::json!({"type": "object"}),
            )];
        agent
            .recompute_fingerprint()
            .expect("fingerprint the publication");
        agent
    }

    fn install(
        delivery: &SessionRunDelivery,
        session: &str,
        inputs: Vec<ResolvedInput>,
        skills: Vec<ResolvedSkillBinding>,
    ) {
        let mut frozen = projection(1, "provider/model", Some(reviewing_agent()));
        frozen.resources = ResolvedSessionResources::try_new(inputs, skills).expect("resources");
        delivery
            .projections()
            .install(session, frozen)
            .expect("install the projection");
    }

    async fn reserved(
        delivery: &SessionRunDelivery,
        session: &str,
    ) -> Option<Box<awaken_dispatch_contract::RunDispatch>> {
        match delivery
            .dispatch
            .observe_session_run_admission(&RunId(format!("{session}-run")))
            .await
            .expect("observe the reservation")
        {
            SessionRunAdmissionObservation::Live(request) => Some(request),
            _ => None,
        }
    }

    #[tokio::test]
    async fn coordinator_reservation_follows_its_decision_table() {
        let stored = skill();

        // C1: no installed projection is a retryable refusal.
        let delivery = delivery(Some(stored.clone()));
        let refused = delivery
            .reserve_session_run(command("cold", "hello"))
            .await
            .expect_err("C1");
        assert_eq!(refused.code, "session_projection_not_installed", "C1");
        assert!(
            reserved(&delivery, "cold").await.is_none(),
            "C1 nothing reserved"
        );

        // C2: a MemoryStore binding that changed since the Session froze it.
        install(&delivery, "stale", vec![memory_input(3)], Vec::new());
        delivery
            .reserve_session_run(command("stale", "hello"))
            .await
            .expect_err("C2");
        assert!(
            reserved(&delivery, "stale").await.is_none(),
            "C2 nothing reserved"
        );

        // C4: a pinned bundle the store does not hold, then one whose digest
        // is not the pin.
        let missing = delivery_without_store();
        install(&missing, "missing", Vec::new(), vec![pin(&stored)]);
        missing
            .reserve_session_run(command("missing", "/review the patch"))
            .await
            .expect_err("C4 missing");
        assert!(
            reserved(&missing, "missing").await.is_none(),
            "C4 nothing reserved"
        );
        let mut repinned = pin(&stored);
        repinned.bundle_sha256 = "sha256:other".into();
        install(&delivery, "repinned", Vec::new(), vec![repinned]);
        delivery
            .reserve_session_run(command("repinned", "/review the patch"))
            .await
            .expect_err("C4 mismatched");
        assert!(
            reserved(&delivery, "repinned").await.is_none(),
            "C4 nothing reserved"
        );

        // C5: every input holds. The row is unclaimable until activation,
        // placed on a registered Worker, carries the desired manifest whose
        // Skill binding pins the bundle digest, the frozen model, and the
        // slash command expanded from the verified bundle.
        install(
            &delivery,
            "ready",
            vec![memory_input(4)],
            vec![pin(&stored)],
        );
        assert_eq!(
            delivery
                .reserve_session_run(command("ready", "/review the patch"))
                .await
                .expect("C5"),
            SessionRunReservation::Reserved,
            "C5"
        );
        let row = reserved(&delivery, "ready").await.expect("C5 reserved row");
        assert_eq!(
            row.activation.model_ref_override.as_deref(),
            Some("provider/model"),
            "C5 frozen model"
        );
        let input = row.activation.input[0].text_content();
        assert!(
            input.contains("Review the patch carefully."),
            "C5 expansion: {input}"
        );
        let resources = row
            .session_resources
            .as_ref()
            .expect("C5 Resource envelope");
        assert!(
            resources
                .resolved_resources_json
                .contains(stored.bundle_sha256.as_str()),
            "C5 the envelope pins the bundle digest"
        );
    }

    fn delivery_without_store() -> SessionRunDelivery {
        delivery(None)
    }
}
