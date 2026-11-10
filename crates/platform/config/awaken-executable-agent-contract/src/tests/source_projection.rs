//! Source projection shares current/history and the single public grammar.
use super::*;
use awaken_agent_contract::{AcpSessionConfiguration, ModelTarget};
use awaken_runtime_contract::resolved::{ModelProvisioning, ResolvedModelCandidate};

struct OpaqueSourceProjector;
impl PublishedModelReferenceProjector for OpaqueSourceProjector {
    fn project(
        &self,
        candidate: &ResolvedModelCandidate,
    ) -> Result<Option<ModelSelection>, ManagedModelIdError> {
        if let ModelProvisioning::Provider {
            route_ref,
            endpoint,
            ..
        } = candidate.provisioning()
            && route_ref.starts_with("opaque-source-")
        {
            return Ok(Some(ModelSelection::Target {
                target: ModelTarget {
                    model_id: candidate.binding().identity().1.into(),
                    provider_id: Some("vendor".into()),
                    api_dialect: Some(endpoint.api_dialect.clone()),
                    protocol_endpoint_id: None,
                    endpoint_name: Some("logical-broker-surface".into()),
                },
                backend_ref: "genai".into(),
                configuration: AcpSessionConfiguration::default(),
            }));
        }
        CanonicalPublishedModelReferenceProjector.project(candidate)
    }
}

#[test]
fn source_owned_projection_keeps_history_time_and_separates_same_named_config_routes() {
    // C1 legitimate source refs are opaque to Config; C2 a current/historical
    // source pair denotes one logical Offering; C3 another Config endpoint
    // serves the same model. R1 no source adapter => explicit refusal; R2 the
    // adapter + same grammar => two selectable IDs; R3 source history gives
    // its original time, with no raw account/route id or snapshot rewrite.
    let old = provider_registration("opaque-source-first", "genai", 10);
    let current = provider_registration("opaque-source-last", "genai", 20);
    let config = provider_registration("workspace:endpoint@1", "genai", 30);
    let original = current.clone();
    assert!(
        project_current_model_publications(
            [&current],
            [&old, &current],
            &CanonicalPublishedModelReferenceProjector
        )
        .is_err(),
        "R1"
    );
    let rows = project_current_model_publications(
        [&current, &config],
        [&old, &current, &config],
        &OpaqueSourceProjector,
    )
    .unwrap();
    assert_eq!(rows.len(), 2, "R2");
    let source = rows
        .iter()
        .find(|r| {
            r.model_reference
                .contains("endpoint=logical-broker-surface")
        })
        .unwrap();
    assert_eq!(source.first_published_at_unix_ms, Some(10), "R3");
    assert!(
        !source.model_reference.contains("opaque-source")
            && !source.model_reference.contains("shared-account"),
        "R3"
    );
    let ModelSelection::Target { target, .. } =
        parse_managed_model_id(&source.model_reference).unwrap()
    else {
        panic!("R2 existing Target")
    };
    assert_eq!(target.provider_id.as_deref(), Some("vendor"));
    assert_eq!(target.api_dialect.as_deref(), Some("open_ai_chat"));
    assert_eq!(
        target.endpoint_name.as_deref(),
        Some("logical-broker-surface")
    );
    assert_eq!(current, original, "R3 immutable registration");
}

#[test]
fn source_projector_cannot_bypass_the_existing_public_grammar() {
    // C1 a source projector supplies Auto (unrepresentable), or a Target with
    // an internal protocol endpoint id; C2 registration is otherwise valid. Both rows
    // refuse the entire directory, with no plausible empty/sanitized choice.
    struct Invalid(bool);
    impl PublishedModelReferenceProjector for Invalid {
        fn project(
            &self,
            _: &ResolvedModelCandidate,
        ) -> Result<Option<ModelSelection>, ManagedModelIdError> {
            Ok(Some(if self.0 {
                ModelSelection::Auto
            } else {
                ModelSelection::Target {
                    target: ModelTarget {
                        model_id: "model".into(),
                        provider_id: Some("vendor".into()),
                        api_dialect: None,
                        protocol_endpoint_id: Some("internal".into()),
                        endpoint_name: None,
                    },
                    backend_ref: "genai".into(),
                    configuration: Default::default(),
                }
            }))
        }
    }
    let current = provider_registration("workspace:endpoint@1", "genai", 20);
    for auto in [false, true] {
        assert!(
            project_published_model_reference(
                &current.snapshot.resolved_spec.model_binding,
                &Invalid(auto),
            )
            .is_err(),
            "the shared single-candidate gate is the directory grammar gate"
        );
        assert!(
            project_current_model_publications([&current], [&current], &Invalid(auto)).is_err()
        );
    }
}

#[test]
fn one_account_pool_contributes_one_id_but_distinct_publications_retain_their_time_evidence() {
    // D1 one registration has two account candidates for one public Offering;
    // D2 its legacy sequence is not verified; D3 a distinct publication of the
    // same ID is also unverified. R1 D1+D2 -> one ID and factual time 10;
    // R2 D3 -> unknown time, not min(clock). R3 the later row becomes fenced ->
    // original first time 10. Deduplication is local to each registration only.
    let mut pool = provider_registration("opaque-source-first", "genai", 10);
    let primary = &pool.snapshot.resolved_spec.model_binding;
    let second = ResolvedModelCandidate::try_from_parts(
        awaken_runtime_contract::resolved::ModelBinding::parse(
            "second-account",
            "shared/model",
            "genai",
        )
        .unwrap(),
        primary.provisioning().clone(),
    )
    .unwrap();
    pool.snapshot.resolved_spec.model_candidates.push(second);
    pool.publication_order_verified = false;
    let ids = registration_model_references(&pool, &OpaqueSourceProjector).unwrap();
    assert_eq!(
        ids,
        vec!["shared/model;provider=vendor;api=open_ai_chat;endpoint=logical-broker-surface"],
        "R1"
    );
    let rows =
        project_current_model_publications([&pool], [&pool], &OpaqueSourceProjector).unwrap();
    assert_eq!(
        rows[0].first_published_at_unix_ms,
        Some(10),
        "R1 one publication"
    );
    let mut later = pool.clone();
    later.source_revision += 1;
    later.snapshot.metadata.source.revision = later.source_revision;
    later.snapshot.recompute_fingerprint().unwrap();
    later.publication_sequence = Some(20);
    later.published_at_unix_ms = Some(5);
    let rows =
        project_current_model_publications([&later], [&pool, &later], &OpaqueSourceProjector)
            .unwrap();
    assert_eq!(
        rows[0].first_published_at_unix_ms, None,
        "R2 distinct unfenced publications"
    );
    later.publication_order_verified = true;
    let rows =
        project_current_model_publications([&later], [&pool, &later], &OpaqueSourceProjector)
            .unwrap();
    assert_eq!(
        rows[0].first_published_at_unix_ms,
        Some(10),
        "R3 sequence, not later clock"
    );
}

#[test]
fn registration_deduplication_preserves_first_occurrence_while_the_directory_stays_sorted() {
    // O1 primary z with fallback a then duplicate z -> [z,a]; O2 primary a
    // with fallback z then duplicate a -> [a,z]. The public helper keeps the
    // original primary/fallback traversal after local deduplication. In both
    // rows the existing directory boundary independently remains sorted [a,z].
    let binding = |model| {
        awaken_runtime_contract::resolved::ModelBinding::parse("provider", model, "genai").unwrap()
    };
    for (primary, fallbacks, expected) in [
        (
            "z-model",
            ["a-model", "z-model"],
            ["z-model;provider=provider", "a-model;provider=provider"],
        ),
        (
            "a-model",
            ["z-model", "a-model"],
            ["a-model;provider=provider", "z-model;provider=provider"],
        ),
    ] {
        let mut entry = registration();
        entry.snapshot.resolved_spec.model_binding = ResolvedModelCandidate::host(binding(primary));
        entry.snapshot.resolved_spec.model_candidates = fallbacks
            .into_iter()
            .map(|model| ResolvedModelCandidate::host(binding(model)))
            .collect();
        assert_eq!(
            registration_model_references(&entry, &CanonicalPublishedModelReferenceProjector)
                .unwrap(),
            expected
        );
        let directory = project_current_model_publications(
            [&entry],
            [&entry],
            &CanonicalPublishedModelReferenceProjector,
        )
        .unwrap();
        assert_eq!(
            directory
                .into_iter()
                .map(|row| row.model_reference)
                .collect::<Vec<_>>(),
            ["a-model;provider=provider", "z-model;provider=provider"]
        );
    }
}
