// Private fixture builders shared by the conformance rule families. Included
// once at crate root so fixture identity and credential setup remain canonical.
fn dispatch(ns: &str, run: &str, thread: &str) -> RunDispatch {
    RunDispatch::new(RunActivation::new(
        run_id(ns, run),
        thread_id(ns, thread),
        ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId(format!("{ns}-snapshot")),
            metadata: Default::default(),
            root_agent_id: AgentId(format!("{ns}-agent")),
            resolved_spec: ResolvedSpec {
                model_candidates: Vec::new(),
                catalog_fingerprint: CatalogFingerprint(format!("{ns}-fingerprint")),
                instructions: "conformance".to_string(),
                max_steps: 4,
                delegation_limits: Default::default(),
                model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                    ModelBinding::new("provider", "model", "backend"),
                ),
                tool_descriptors: Vec::new(),
                plugin_ids: Vec::new(),
                plugin_config: Default::default(),
                context_policy: Default::default(),
                tool_presentation: Default::default(),
            },
            fingerprint: CatalogFingerprint(format!("{ns}-fingerprint")),
        },
        vec![Message::text(
            MessageId(format!("{ns}-{run}-input")),
            Role::User,
            "run conformance",
        )],
    ))
}

fn credential_dispatch(ns: &str, run: &str, thread: &str, holder: &PlaintextHolder) -> RunDispatch {
    let mut request = dispatch(ns, run, thread);
    request.activation.snapshot.resolved_spec.model_binding =
        awaken_runtime_contract::resolved::ResolvedModelCandidate::try_provider(
            ModelBinding::new(format!("{ns}-provider"), "model", "genai"),
            format!("{ns}-provider@1"),
            format!("{ns}-route@1"),
            ns,
            Some(
                CredentialAccess::new(
                    CredentialRef {
                        id: format!("{ns}-credential"),
                        revision: 7,
                    },
                    CredentialMaterialSource::ControlPlaneReference,
                    CredentialUsage::ProviderAdapter,
                    CredentialExecutionPolicy::exact(
                        holder.clone(),
                        ModelExposurePolicy::Forbidden,
                    ),
                )
                .with_target(awaken_runtime_contract::CredentialTarget::new(
                    awaken_runtime_contract::credential::CredentialPurpose::ProviderAdapter,
                    format!("{ns}-provider"),
                )),
            ),
            InferenceEndpoint {
                adapter_kind: "openai".into(),
                api_dialect: "open_ai_chat".into(),
                base_url: "https://provider.invalid/v1".into(),
                upstream_model: "model".into(),
                processing_placement: None,
            },
        )
        .expect("testkit provider candidate is coherent");
    request.inference_plaintext_holder = Some(holder.clone());
    request
}

fn ready_dispatch_worker(ns: &str, suffix: &str) -> WorkerSnapshot {
    let manifest = WorkerManifest::default();
    WorkerSnapshot {
        identity: WorkerIdentity::new(format!("{ns}-{suffix}-worker"), "boot", 1),
        capability_fingerprint: manifest
            .fingerprint()
            .expect("default Worker manifest fingerprints"),
        manifest,
        state: WorkerState::Ready,
        in_flight: 0,
        warm_environment_shapes: Default::default(),
        credential_observations: Default::default(),
        acp_capability_observations: Default::default(),
        // This fixture models a Worker that remains Ready for the whole
        // conformance scenario. Memory/SQLite consume the scenario's logical
        // clock, while PostgreSQL deliberately consumes its authoritative
        // database clock; a small synthetic deadline would therefore test an
        // expired Worker on PostgreSQL instead of the intended claim policy.
        expires_at_ms: u64::MAX,
    }
}

fn credential_worker(ns: &str, holder: &PlaintextHolder, capable: bool) -> WorkerSnapshot {
    let mut manifest = WorkerManifest::default();
    if capable {
        let realization = CredentialRealizationCapabilities {
            holders: [holder.clone()].into_iter().collect(),
            material_sources: [CredentialMaterialSource::ControlPlaneReference]
                .into_iter()
                .collect(),
            realization_kinds: [CredentialRealizationKind::WorkerProviderAdapter]
                .into_iter()
                .collect(),
            recipient_bound_envelopes: false,
            extension_consumers: Default::default(),
            alternatives: Vec::new(),
        };
        manifest.capabilities.insert(
            realization
                .manifest_capability()
                .expect("credential capabilities serialize")
                .expect("non-empty credential capabilities emit one manifest entry"),
        );
    }
    WorkerSnapshot {
        identity: WorkerIdentity::new(format!("{ns}-credential-worker-{capable}"), "boot", 1),
        capability_fingerprint: manifest
            .fingerprint()
            .expect("credential Worker manifest fingerprints"),
        manifest,
        state: WorkerState::Ready,
        in_flight: 0,
        warm_environment_shapes: Default::default(),
        credential_observations: Default::default(),
        acp_capability_observations: Default::default(),
        // Credential conformance varies the immutable manifest axis only. A
        // backend-authoritative wall clock must not turn that partition into
        // the unrelated "expired Worker" rejection.
        expires_at_ms: u64::MAX,
    }
}

fn run_id(ns: &str, suffix: &str) -> RunId {
    RunId(format!("{ns}-{suffix}"))
}

fn thread_id(ns: &str, suffix: &str) -> ThreadId {
    ThreadId(format!("{ns}-{suffix}"))
}
