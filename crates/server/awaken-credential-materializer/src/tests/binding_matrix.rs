//! T15-C01 binding matrix through the public materialization entrypoints.
//!
//! Baseline B: credential C1 revision 1 in workspace-a, scoped to provider
//! `anthropic`, policy holder H1 (self-hosted Worker), target audience
//! `anthropic`, current runtime incarnation (attempt ownership current).
//! Every other cell changes exactly ONE binding dimension:
//!
//! | Cell | changed dimension | entrypoint | expected |
//! |---|---|---|---|
//! | B/direct | - | resolve_for_workspace_and_provider | material, reads+1 |
//! | B/claimed | - | materialize_claimed_provider | material, reads+1 |
//! | D1 | revision 1->2 | resolve_for_workspace_and_provider | RevisionMismatch, reads+0 |
//! | D2 | selected holder H1->H2 | resolve_for_workspace_and_provider | refused, reads+0 (variant: see reproducer) |
//! | D3 | access target audience | resolve_for_workspace_and_provider | BindingMismatch, reads+0 |
//! | D4 | scope: workspace a->b | resolve_for_workspace_and_provider | RecipientMismatch, reads+0 |
//! | D5 | scope: provider | resolve_for_workspace_and_provider | RecipientMismatch, reads+0 |
//! | D6 | runtime incarnation lost | materialize_claimed_provider | ownership-lost error, reads+0 |
//!
//! Every rejection cell runs twice; the error must be identical (stable) and
//! the per-dimension rejection count is 2 with zero plaintext reads.
//!
//! Negative control: an "id-only" cell (credential C2, same revision/holder/
//! target/scope) changes only the credential id. It is executed (it resolves
//! C2's own material) and must not be counted as covering revision/holder.

use std::collections::{BTreeMap, BTreeSet};

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Dimension {
    CredentialId,
    Revision,
    Holder,
    Target,
    WorkspaceScope,
    ProviderScope,
    RuntimeIncarnation,
}

const BINDING_DIMENSIONS: [Dimension; 6] = [
    Dimension::Revision,
    Dimension::Holder,
    Dimension::Target,
    Dimension::WorkspaceScope,
    Dimension::ProviderScope,
    Dimension::RuntimeIncarnation,
];

#[derive(Debug, Clone)]
struct Cell {
    name: &'static str,
    credential_id: String,
    revision: u64,
    holder: PlaintextHolder,
    target: &'static str,
    workspace: &'static str,
    provider: &'static str,
    incarnation_current: bool,
    claimed: bool,
}

fn changed(baseline: &Cell, cell: &Cell) -> BTreeSet<Dimension> {
    let mut dimensions = BTreeSet::new();
    if cell.credential_id != baseline.credential_id {
        dimensions.insert(Dimension::CredentialId);
    }
    if cell.revision != baseline.revision {
        dimensions.insert(Dimension::Revision);
    }
    if cell.holder != baseline.holder {
        dimensions.insert(Dimension::Holder);
    }
    if cell.target != baseline.target {
        dimensions.insert(Dimension::Target);
    }
    if cell.workspace != baseline.workspace {
        dimensions.insert(Dimension::WorkspaceScope);
    }
    if cell.provider != baseline.provider {
        dimensions.insert(Dimension::ProviderScope);
    }
    if cell.incarnation_current != baseline.incarnation_current {
        dimensions.insert(Dimension::RuntimeIncarnation);
    }
    dimensions
}

/// Binding dimensions covered by a set of cells that were each REJECTED and
/// each changed exactly one dimension relative to the baseline.
fn coverage(baseline: &Cell, rejected: &[&Cell]) -> BTreeSet<Dimension> {
    rejected
        .iter()
        .map(|cell| changed(baseline, cell))
        .filter(|dimensions| dimensions.len() == 1)
        .flatten()
        .filter(|dimension| BINDING_DIMENSIONS.contains(dimension))
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CellError {
    Typed(CredentialMaterialError),
    Claim(String),
}

struct Fixture {
    materializer: PinnedCredentialMaterializer,
    secrets: Arc<CountingSecrets>,
    primary_id: String,
    secondary_id: String,
}

async fn fixture() -> Fixture {
    let credentials = Arc::new(InMemoryCredentialRepo::new());
    let secrets = Arc::new(CountingSecrets::new());
    let mut ids = Vec::new();
    for secret in ["matrix-secret-primary", "matrix-secret-secondary"] {
        let source = enter_credential(
            CredentialCreateParams {
                workspace_id: "workspace-a".into(),
                kind: CredentialKind::Vault,
                provider_id: Some("anthropic".into()),
                env_key: None,
                secret: Some(RedactedString::new(secret)),
                oauth_command: None,
            },
            secrets.as_ref(),
            credentials.as_ref(),
            awaken_time::system_clock().as_ref(),
        )
        .await
        .expect("matrix credential");
        assert_eq!(source.version, 1, "fixture rows start at revision 1");
        ids.push(source.id.0);
    }
    Fixture {
        materializer: pinned_vault(credentials, secrets.clone(), awaken_time::system_clock()),
        secrets,
        secondary_id: ids.pop().unwrap(),
        primary_id: ids.pop().unwrap(),
    }
}

fn worker_holder() -> PlaintextHolder {
    selected_holder()
}

fn other_holder() -> PlaintextHolder {
    PlaintextHolder::new(
        PlaintextBoundary::Workload,
        awaken_agent_contract::credential::SELF_HOSTED_ACP_TRUST_DOMAIN,
    )
}

fn access_for(cell: &Cell) -> CredentialAccess {
    CredentialAccess::new(
        CredentialRef {
            id: cell.credential_id.clone(),
            revision: cell.revision,
        },
        CredentialMaterialSource::ControlPlaneReference,
        CredentialUsage::ProviderAdapter,
        // The published policy always names the baseline holder H1; the
        // holder dimension varies the holder selected at realization.
        CredentialExecutionPolicy::exact(worker_holder(), ModelExposurePolicy::Forbidden),
    )
    .with_target(awaken_agent_contract::credential::CredentialTarget::new(
        awaken_agent_contract::credential::CredentialPurpose::ProviderAdapter,
        cell.target,
    ))
}

struct FixedOwnership(std::result::Result<(), awaken_runtime_contract::AttemptOwnershipError>);

#[async_trait::async_trait]
impl awaken_runtime_contract::AttemptOwnershipVerifier for FixedOwnership {
    async fn verify_current(&self) -> Result<(), awaken_runtime_contract::AttemptOwnershipError> {
        self.0.clone()
    }
}

#[async_trait::async_trait]
impl awaken_credential_contract::CredentialRealizationRecorder for FixedOwnership {
    async fn record(
        &self,
        _receipt: awaken_credential_contract::CredentialRealizationReceipt,
    ) -> Result<(), awaken_credential_contract::CredentialRealizationRecordError> {
        Ok(())
    }
}

fn provider_candidate(cell: &Cell) -> ResolvedModelCandidate {
    ResolvedModelCandidate::try_provider(
        ModelBinding::parse("anthropic", "model", "native").expect("valid binding"),
        "anthropic@1",
        "endpoint@1",
        cell.workspace,
        Some(access_for(cell)),
        InferenceEndpoint {
            adapter_kind: "anthropic".into(),
            api_dialect: "anthropic_messages".into(),
            base_url: "https://example.invalid".into(),
            upstream_model: "model".into(),
            processing_placement: None,
        },
    )
    .expect("coherent matrix provider candidate")
}

async fn run_cell(fixture: &Fixture, cell: &Cell) -> Result<String, CellError> {
    if !cell.claimed {
        return fixture
            .materializer
            .resolve_for_workspace_and_provider(
                &access_for(cell),
                &cell.holder,
                CredentialRealizationKind::WorkerProviderAdapter,
                cell.workspace,
                cell.provider,
                &cell.provider,
            )
            .await
            .map(|resolved| {
                resolved
                    .material
                    .single_secret()
                    .unwrap()
                    .expose_secret()
                    .to_string()
            })
            .map_err(CellError::Typed);
    }
    let candidate = provider_candidate(cell);
    let capabilities = CredentialRealizationCapabilities {
        holders: [cell.holder.clone()].into_iter().collect(),
        material_sources: [CredentialMaterialSource::ControlPlaneReference]
            .into_iter()
            .collect(),
        realization_kinds: [CredentialRealizationKind::WorkerProviderAdapter]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let bindings = awaken_credential_contract::compile_candidate_credential_bindings(
        &[&candidate],
        Some(&cell.holder),
        &capabilities,
        3,
        0,
    )
    .expect("matrix claim bindings");
    let ownership = Arc::new(FixedOwnership(if cell.incarnation_current {
        Ok(())
    } else {
        Err(awaken_runtime_contract::AttemptOwnershipError::Lost)
    }));
    let context = awaken_runtime_contract::RuntimeRunContext::new()
        .with_ownership(ownership)
        .with_credential_realization(
            awaken_credential_contract::AttemptCredentialRealization::new(
                bindings,
                Arc::new(FixedOwnership(Ok(()))),
            ),
        );
    fixture
        .materializer
        .materialize_claimed_provider(
            &candidate,
            &context,
            CredentialRealizationKind::WorkerProviderAdapter,
        )
        .await
        .map(|secret| {
            secret
                .expect("credential-bearing candidate")
                .expose_secret()
                .to_string()
        })
        .map_err(CellError::Claim)
}

fn cells(fixture: &Fixture) -> (Cell, Cell, Vec<(Cell, Option<CellError>)>, Cell) {
    let baseline = Cell {
        name: "B/direct",
        credential_id: fixture.primary_id.clone(),
        revision: 1,
        holder: worker_holder(),
        target: "anthropic",
        workspace: "workspace-a",
        provider: "anthropic",
        incarnation_current: true,
        claimed: false,
    };
    let claimed_baseline = Cell {
        name: "B/claimed",
        claimed: true,
        ..baseline.clone()
    };
    let rejections = vec![
        (
            Cell {
                name: "D1 revision",
                revision: 2,
                ..baseline.clone()
            },
            Some(CellError::Typed(CredentialMaterialError::RevisionMismatch)),
        ),
        (
            Cell {
                name: "D2 holder",
                holder: other_holder(),
                ..baseline.clone()
            },
            // Variant asserted by the product-gap reproducer below.
            None,
        ),
        (
            Cell {
                name: "D3 target",
                target: "provider-b",
                ..baseline.clone()
            },
            Some(CellError::Typed(CredentialMaterialError::BindingMismatch)),
        ),
        (
            Cell {
                name: "D4 workspace scope",
                workspace: "workspace-b",
                ..baseline.clone()
            },
            Some(CellError::Typed(CredentialMaterialError::RecipientMismatch)),
        ),
        (
            Cell {
                name: "D5 provider scope",
                provider: "other-provider",
                ..baseline.clone()
            },
            Some(CellError::Typed(CredentialMaterialError::RecipientMismatch)),
        ),
        (
            Cell {
                name: "D6 runtime incarnation",
                incarnation_current: false,
                ..claimed_baseline.clone()
            },
            None,
        ),
    ];
    let id_only = Cell {
        name: "NC id-only",
        credential_id: fixture.secondary_id.clone(),
        ..baseline.clone()
    };
    (baseline, claimed_baseline, rejections, id_only)
}

#[tokio::test]
async fn binding_matrix_rejects_each_dimension_before_plaintext_with_stable_class() {
    let fixture = fixture().await;
    let (baseline, claimed_baseline, rejections, id_only) = cells(&fixture);

    // Success cells prove the read counter is wired to the plaintext edge.
    for success in [&baseline, &claimed_baseline] {
        let before = fixture.secrets.reads();
        assert_eq!(
            run_cell(&fixture, success).await.expect(success.name),
            "matrix-secret-primary",
            "{}",
            success.name
        );
        assert_eq!(
            fixture.secrets.reads(),
            before + 1,
            "{} reads+1",
            success.name
        );
    }

    let lost = FixedOwnership(Err(awaken_runtime_contract::AttemptOwnershipError::Lost));
    let lost_display = awaken_runtime_contract::execution::verify_attempt_ownership(Some(
        &lost as &dyn awaken_runtime_contract::AttemptOwnershipVerifier,
    ))
    .await
    .unwrap_err()
    .to_string();
    let mut rejection_counts = BTreeMap::<Dimension, usize>::new();
    for (cell, expected) in &rejections {
        let dimensions = changed(&baseline, cell);
        assert_eq!(
            dimensions.len(),
            1,
            "{} changes exactly one dimension",
            cell.name
        );
        let dimension = *dimensions.iter().next().unwrap();
        assert!(BINDING_DIMENSIONS.contains(&dimension), "{}", cell.name);
        let mut observed = Vec::new();
        for _ in 0..2 {
            let before = fixture.secrets.reads();
            let error = run_cell(&fixture, cell).await.expect_err(cell.name);
            assert_eq!(
                fixture.secrets.reads(),
                before,
                "{} rejected before plaintext read",
                cell.name
            );
            assert!(
                !format!("{error:?}").contains("matrix-secret"),
                "{} error is secret-free",
                cell.name
            );
            *rejection_counts.entry(dimension).or_default() += 1;
            observed.push(error);
        }
        assert_eq!(
            observed[0], observed[1],
            "{} stable classification",
            cell.name
        );
        match expected {
            Some(expected) => assert_eq!(&observed[0], expected, "{}", cell.name),
            None if dimension == Dimension::RuntimeIncarnation => assert_eq!(
                observed[0],
                CellError::Claim(lost_display.clone()),
                "{} ownership-lost class",
                cell.name
            ),
            None => {}
        }
    }
    assert_eq!(
        rejection_counts,
        BINDING_DIMENSIONS
            .iter()
            .map(|dimension| (*dimension, 2))
            .collect::<BTreeMap<_, _>>(),
        "per-dimension rejection counts"
    );
    let rejected = rejections.iter().map(|(cell, _)| cell).collect::<Vec<_>>();
    assert_eq!(
        coverage(&baseline, &rejected),
        BINDING_DIMENSIONS.into_iter().collect::<BTreeSet<_>>(),
        "matrix covers every binding dimension"
    );

    // Negative control: the id-only cell is executed and does not cover
    // revision/holder (or anything else).
    assert_eq!(
        changed(&baseline, &id_only),
        BTreeSet::from([Dimension::CredentialId]),
        "NC id-only changes only the id"
    );
    let before = fixture.secrets.reads();
    assert_eq!(
        run_cell(&fixture, &id_only)
            .await
            .expect("NC id-only resolves its own row"),
        "matrix-secret-secondary"
    );
    assert_eq!(
        fixture.secrets.reads(),
        before + 1,
        "NC id-only reads its own row"
    );
    let id_only_coverage = coverage(&baseline, &[&id_only]);
    assert!(
        !id_only_coverage.contains(&Dimension::Revision)
            && !id_only_coverage.contains(&Dimension::Holder),
        "NC: an id-only swap must not count as revision/holder coverage"
    );
    assert!(
        !BINDING_DIMENSIONS
            .into_iter()
            .collect::<BTreeSet<_>>()
            .is_subset(&id_only_coverage),
        "NC: id-only matrix is insufficient"
    );
}

#[tokio::test]
async fn binding_matrix_holder_dimension_has_its_recipient_classification() {
    let fixture = fixture().await;
    let (baseline, _, rejections, _) = cells(&fixture);
    let (holder_cell, _) = rejections
        .iter()
        .find(|(cell, _)| changed(&baseline, cell) == BTreeSet::from([Dimension::Holder]))
        .expect("holder cell");
    let before = fixture.secrets.reads();
    let error = run_cell(&fixture, holder_cell).await.expect_err("holder");
    assert_eq!(fixture.secrets.reads(), before, "no plaintext read");
    assert_eq!(
        error,
        CellError::Typed(CredentialMaterialError::RecipientMismatch),
        "holder mismatch must keep the same class as resolve_validated's R4"
    );
}
