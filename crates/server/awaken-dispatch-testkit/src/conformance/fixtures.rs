// Private fixture builders shared by the conformance rule families. Included
// once at crate root so fixture identity and credential setup remain canonical.
fn dispatch(ns: &str, run: &str, thread: &str) -> RunDispatch {
    // These conformance rows exercise current local/registered execution.
    // Pin the existing strict builder instead of accidentally granting a
    // legacy zero-version queue envelope through a default ANY Worker.
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
    .with_placement(PlacementRequirements::remote_preferred())
}

/// Attach the current Session command identity after the fixture has applied
/// every application-owned requirement and before tests mutate Runtime
/// projections. The deterministic operation id is test-only input, not a
/// second production identity implementation.
fn current_session_command(request: RunDispatch) -> RunDispatch {
    let operation_id = format!("{}-operation", request.run_id().0);
    current_session_command_with_operation(request, operation_id)
}

fn current_session_command_with_operation(
    request: RunDispatch,
    operation_id: impl Into<String>,
) -> RunDispatch {
    let command = AdmitSessionRun {
        session_id: request
            .session_thread_id
            .as_ref()
            .unwrap_or_else(|| request.thread_id())
            .0
            .clone(),
        agent_id: request.activation.snapshot.root_agent_id.0.clone(),
        operation_id: operation_id.into(),
        run_id: request.run_id().clone(),
        messages: request.activation.input.clone(),
        data_subject_id: request
            .activation
            .data_subject_id
            .as_ref()
            .map(|subject| subject.0.clone()),
        traceparent: request.traceparent.clone(),
        execution_requirements: SessionRunExecutionRequirements {
            tool_capability_narrowing: request.activation.tool_capability_narrowing,
            required_worker_capabilities: request.placement.required_capabilities.clone(),
        },
        replacement: request.session_run_replacement,
    };
    request.with_session_command_fingerprint(SessionRunCommandFingerprint::current(&command))
}

fn credential_dispatch(ns: &str, run: &str, thread: &str, holder: &PlaintextHolder) -> RunDispatch {
    let mut request = dispatch(ns, run, thread);
    request.activation.snapshot.resolved_spec.model_binding = credential_candidate(
        ns,
        &format!("{ns}-provider"),
        "model",
        &format!("{ns}-credential"),
        &format!("{ns}-route@1"),
        holder,
    );
    request.inference_plaintext_holder = Some(holder.clone());
    request
}

fn remote_credential_dispatch(
    ns: &str,
    run: &str,
    thread: &str,
    holder: &PlaintextHolder,
) -> RunDispatch {
    let endpoint = format!("https://{ns}.agent.invalid");
    let mut request = dispatch(ns, run, thread);
    request.activation.snapshot.resolved_spec.model_binding =
        awaken_runtime_contract::resolved::ResolvedModelCandidate::try_remote(
            ModelBinding::new("", "", format!("a2a:{endpoint}")),
            ns,
            Some(
                CredentialAccess::new(
                    CredentialRef {
                        id: format!("{ns}-remote-credential"),
                        revision: 7,
                    },
                    CredentialMaterialSource::ControlPlaneReference,
                    CredentialUsage::HttpHeader {
                        name: "authorization".into(),
                        scheme: Some("Bearer".into()),
                    },
                    CredentialExecutionPolicy::exact(
                        holder.clone(),
                        ModelExposurePolicy::Forbidden,
                    ),
                )
                .with_target(awaken_runtime_contract::CredentialTarget::new(
                    awaken_runtime_contract::credential::CredentialPurpose::RemoteAgentAuthorization,
                    endpoint,
                )),
            ),
            format!("sha256:{ns}-agent-card"),
        )
        .expect("testkit remote candidate is coherent");
    request.inference_plaintext_holder = Some(holder.clone());
    request
}

fn credential_candidate(
    workspace_id: &str,
    provider_id: &str,
    model_id: &str,
    credential_id: &str,
    route_ref: &str,
    holder: &PlaintextHolder,
) -> awaken_runtime_contract::resolved::ResolvedModelCandidate {
    awaken_runtime_contract::resolved::ResolvedModelCandidate::try_provider(
        ModelBinding::new(provider_id, model_id, "genai"),
        format!("{provider_id}@1"),
        route_ref,
        workspace_id,
        Some(
            CredentialAccess::new(
                CredentialRef {
                    id: credential_id.to_string(),
                    revision: 7,
                },
                CredentialMaterialSource::ControlPlaneReference,
                CredentialUsage::ProviderAdapter,
                CredentialExecutionPolicy::exact(holder.clone(), ModelExposurePolicy::Forbidden),
            )
            .with_target(awaken_runtime_contract::CredentialTarget::new(
                awaken_runtime_contract::credential::CredentialPurpose::ProviderAdapter,
                provider_id,
            )),
        ),
        InferenceEndpoint {
            adapter_kind: "openai".into(),
            api_dialect: "open_ai_chat".into(),
            base_url: "https://provider.invalid/v1".into(),
            upstream_model: model_id.into(),
            processing_placement: None,
        },
    )
    .expect("testkit provider candidate is coherent")
}

fn ready_dispatch_worker(ns: &str, suffix: &str) -> WorkerSnapshot {
    // Protocol support is a fixed precondition; each caller varies the actual
    // claim/credential/recovery cause under test.
    let mut manifest = WorkerManifest::default();
    manifest.runtime_protocol.min = 1;
    manifest.runtime_protocol.max = awaken_dispatch_contract::CURRENT_RUNTIME_PROTOCOL_VERSION;
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
    credential_worker_for(
        ns,
        holder,
        capable.then_some(CredentialRealizationKind::WorkerProviderAdapter),
    )
}

fn remote_credential_worker(ns: &str, holder: &PlaintextHolder) -> WorkerSnapshot {
    credential_worker_for(ns, holder, Some(CredentialRealizationKind::WorkerRelay))
}

fn credential_worker_for(
    ns: &str,
    holder: &PlaintextHolder,
    realization_kind: Option<CredentialRealizationKind>,
) -> WorkerSnapshot {
    let mut manifest = ready_dispatch_worker(ns, "credential").manifest;
    let worker_kind = realization_kind
        .map(|kind| format!("{kind:?}"))
        .unwrap_or_else(|| "incapable".into());
    if let Some(realization_kind) = realization_kind {
        let realization = CredentialRealizationCapabilities {
            holders: [holder.clone()].into_iter().collect(),
            material_sources: [CredentialMaterialSource::ControlPlaneReference]
                .into_iter()
                .collect(),
            realization_kinds: [realization_kind].into_iter().collect(),
            recipient_bound_envelopes: false,
            envelope_required: false,
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
        identity: WorkerIdentity::new(format!("{ns}-credential-worker-{worker_kind}"), "boot", 1),
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
