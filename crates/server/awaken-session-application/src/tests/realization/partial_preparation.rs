//! T14-C01 (部分准备拒绝): a two-input manifest replaces an installed one;
//! the second input fails its preparation, or the command carries a stale
//! Resource version.
//!
//! Oracles (groups-3.json order):
//! - O0 no partial root (no half-installed desired manifest remains);
//! - O1 the original manifest stays effective (root pair and frozen projection);
//! - O2 the preparation responsibility keeps recoverable identities
//!   `{session}:{revision}:{binding}`.
//!
//! Negative control: comparing only the returned error, without checking the
//! original root and the preparation responsibility, must not pass. The
//! oracle checker below is run against mutated roots (a partially committed
//! manifest, cleared activations) and must reject each, while the
//! error-only check accepts them.
//!
//! | Row | Cause | Effect |
//! |---|---|---|
//! | P1 | expected Resource revision is stale | `Conflict`; root byte-equal; no Runtime effect |
//! | P2 | second of two inputs fails preparation | rollback; active (1, A); rev-2 identities retained as Failed |
//! | P3 | retry after the fault clears | revision 3 installs; rev-1 identity Released |
use super::*;
use crate::tests::effect_counting_runtime::EffectCountingRuntime;
use awaken_session_state_contract::ActivationState;

const SESSION: &str = "t14-partial-preparation";

fn file_input(binding: &str, file: &str, mount: &str) -> crate::session_resource::ResolvedInput {
    crate::session_resource::ResolvedInput {
        created_at_unix_ms: None,
        updated_at_unix_ms: None,
        binding_id: awaken_resource_contract::BindingId::from(binding),
        source: crate::session_resource::ResolvedInputSource::File {
            file_id: awaken_resource_contract::FileId::from(file),
        },
        mount_path: mount.into(),
        access: awaken_resource_contract::ResourceAccess::ReadOnly,
        instructions: None,
    }
}

fn installed_manifest() -> crate::session_resource::ResolvedSessionResources {
    crate::session_resource::ResolvedSessionResources::try_new(
        vec![file_input("kept", "kept-file", "/workspace/kept")],
        Vec::new(),
    )
    .unwrap()
}

/// New and old manifests cross: `kept` survives, `second` is new.
fn replacement_manifest() -> crate::session_resource::ResolvedSessionResources {
    crate::session_resource::ResolvedSessionResources::try_new(
        vec![
            file_input("kept", "kept-file", "/workspace/kept"),
            file_input("second", "second-file", "/workspace/second"),
        ],
        Vec::new(),
    )
    .unwrap()
}

fn replace(expected_resource_revision: Option<u64>, key: &str) -> ReplaceSessionResourceManifest {
    let resources = replacement_manifest();
    ReplaceSessionResourceManifest {
        request_fingerprint: awaken_agent_contract::stable_fingerprint(&(&resources, key)),
        resources,
        expected_resource_revision,
        idempotency_key: Some(key.into()),
    }
}

fn activation_states(session: &PersistedSession) -> Vec<(String, ActivationState)> {
    let mut states = session
        .resources
        .activations
        .iter()
        .map(|activation| (activation.activation_id.clone(), activation.state))
        .collect::<Vec<_>>();
    states.sort_by(|left, right| left.0.cmp(&right.0));
    states
}

/// The O0-O2 checker. It never looks at the command result.
fn check_original_manifest_retained(session: &PersistedSession) -> Result<(), String> {
    let mut violations = Vec::new();
    if session.resources.pending.is_some() {
        violations.push("O0 a desired manifest remains pending".to_string());
    }
    let (active_revision, active) = session.resources.active_generation();
    if active_revision != 1 || active != &installed_manifest() {
        violations.push(format!(
            "O1 installed pair changed to revision {active_revision}: {active:?}"
        ));
    }
    let expected = vec![
        (format!("{SESSION}:1:kept"), ActivationState::Active),
        (format!("{SESSION}:2:kept"), ActivationState::Failed),
        (format!("{SESSION}:2:second"), ActivationState::Failed),
    ];
    let observed = activation_states(session);
    if observed != expected {
        violations.push(format!(
            "O2 preparation identities {observed:?} != {expected:?}"
        ));
    }
    if violations.is_empty() {
        Ok(())
    } else {
        Err(violations.join("; "))
    }
}

async fn fixture() -> (
    Arc<awaken_session_store::SqliteManagedSessionRepository>,
    Arc<EffectCountingRuntime>,
    SessionApplication,
) {
    let repo = sqlite_session_repository("T14 Session store").await;
    let mut session = persisted(SESSION, false, "idle");
    session
        .resources
        .prepare(&SessionId::from(SESSION), installed_manifest())
        .unwrap();
    session.resources.commit().unwrap();
    create(repo.as_ref(), session).await;
    let runtime = Arc::new(EffectCountingRuntime::default());
    let app = application_with_runtime(
        runtime.clone(),
        repo.clone(),
        Arc::new(RecordingEnvironmentSource::default()),
    );
    (repo, runtime, app)
}

#[tokio::test]
async fn failed_second_input_or_stale_version_keeps_the_original_manifest() {
    let id = SessionId::from(SESSION);
    let (repo, runtime, app) = fixture().await;
    let original = repo.get(&id).await.unwrap();
    assert_eq!(
        activation_states(&original),
        vec![(format!("{SESSION}:1:kept"), ActivationState::Active)],
        "fixture installs generation 1"
    );

    // P1: stale Resource version.
    let stale = app
        .resource_manifests
        .replace_session_resource_manifest(&id, replace(Some(0), "t14-stale"))
        .await;
    assert!(
        matches!(stale, Err(SessionResourceManifestError::Conflict)),
        "P1 stale version conflicts: {stale:?}"
    );
    assert_eq!(
        repo.get(&id).await.unwrap(),
        original,
        "P1/O0 root byte-equal"
    );
    assert_eq!(runtime.counts(), Default::default(), "P1 no Runtime effect");

    // P2: the second input fails its preparation.
    *runtime.fail_apply_binding.lock().unwrap() = Some("second".into());
    let failed = app
        .resource_manifests
        .replace_session_resource_manifest(&id, replace(Some(1), "t14-replace"))
        .await;
    let Err(SessionResourceManifestError::ProjectionAfterCommit { source, .. }) = &failed else {
        panic!("P2 preparation failure surfaces after the desired commit: {failed:?}")
    };
    assert!(
        source.to_string().contains("second"),
        "P2 the failing input is named: {source}"
    );
    assert_eq!(
        runtime.resource_applies.lock().unwrap().as_slice(),
        [(1, 2, vec!["kept".to_string(), "second".to_string()])].as_slice(),
        "P2 exactly one forward attempt of the complete pair"
    );
    assert_eq!(
        runtime.resource_rollbacks.lock().unwrap().as_slice(),
        [(2, 1)].as_slice(),
        "P2 the attempt is rolled back to the installed generation"
    );
    let rolled_back = repo.get(&id).await.unwrap();
    check_original_manifest_retained(&rolled_back).expect("P2/O0-O2");
    assert_eq!(
        rolled_back.resources.revision, 2,
        "P2 monotonic attempt watermark"
    );
    let failed_second = rolled_back
        .resources
        .activations
        .iter()
        .find(|activation| activation.activation_id == format!("{SESSION}:2:second"))
        .expect("P2/O2 second input responsibility");
    assert!(
        failed_second
            .last_error
            .as_deref()
            .is_some_and(|error| error.contains("second")),
        "P2/O2 diagnostic retained: {failed_second:?}"
    );
    let projection = app
        .queries()
        .frozen_session_projection("workspace".into(), &rolled_back, true)
        .await
        .expect("P2/O1 frozen projection");
    assert_eq!(projection.resource_revision, 1, "P2/O1 projection revision");
    assert_eq!(
        projection.resources,
        installed_manifest(),
        "P2/O1 projection manifest"
    );
    assert_eq!(runtime.counts().model_calls, 0, "P2 no execution");

    // P3: the recoverable identity is retried after the fault clears.
    *runtime.fail_apply_binding.lock().unwrap() = None;
    let installed = app
        .resource_manifests
        .replace_session_resource_manifest(&id, replace(Some(2), "t14-retry"))
        .await
        .expect("P3 retry installs the replacement");
    assert!(installed.session.resources.pending.is_none(), "P3");
    let states = activation_states(&repo.get(&id).await.unwrap());
    assert!(
        states.contains(&(format!("{SESSION}:3:kept"), ActivationState::Active))
            && states.contains(&(format!("{SESSION}:3:second"), ActivationState::Active))
            && states.contains(&(format!("{SESSION}:1:kept"), ActivationState::Released)),
        "P3 new generation installs and the original identity is released: {states:?}"
    );
}

#[tokio::test]
async fn error_only_oracle_cannot_accept_partial_or_forgotten_preparation() {
    // NC: reproduce the real failure, then mutate the durable root into the
    // two shapes an error-only check would miss.
    let id = SessionId::from(SESSION);
    let (repo, runtime, app) = fixture().await;
    *runtime.fail_apply_binding.lock().unwrap() = Some("second".into());
    let result = app
        .resource_manifests
        .replace_session_resource_manifest(&id, replace(Some(1), "t14-nc"))
        .await;
    let error_only_oracle = |result: &Result<_, SessionResourceManifestError>| result.is_err();
    assert!(error_only_oracle(&result), "NC the command failed");
    let actual = repo.get(&id).await.unwrap();
    check_original_manifest_retained(&actual).expect("NC baseline root is correct");

    // Mutant M1: the replacement was (partially) committed as active.
    let mut committed = actual.clone();
    committed.resources.active = replacement_manifest();
    committed.resources.committed_revision = Some(2);
    // Mutant M2: the preparation responsibility was dropped.
    let mut cleared = actual.clone();
    cleared.resources.activations.clear();
    // Mutant M3: a half-installed pending manifest remains.
    let mut pending = actual;
    pending.resources.pending = Some(replacement_manifest());
    for (mutant, root) in [("M1", committed), ("M2", cleared), ("M3", pending)] {
        assert!(
            error_only_oracle(&result),
            "NC {mutant}: the error-only oracle still passes"
        );
        let rejection = check_original_manifest_retained(&root)
            .expect_err("NC the root/responsibility checker rejects the mutant");
        assert!(!rejection.is_empty(), "NC {mutant}: {rejection}");
    }
}
