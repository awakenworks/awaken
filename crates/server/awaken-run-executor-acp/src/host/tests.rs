use super::*;
use awaken_lease_testkit::ManualClock;

#[test]
fn local_binding_batch_member_admission_is_exact_and_fail_closed() {
    // Causes: C1 the credential id is absent/present; C2 a present row has the
    // exact CredentialRef. Effects: E1 the member may enter the later atomic
    // extend; E2 the entire batch rejects before mutation. R1-R3 cover the
    // closed quotient; add_bindings evaluates every member under one lock.
    //
    // | Rule | existing | exact credential | admitted |
    // |---|---|---|---|
    // | R1 | no  | n/a | yes |
    // | R2 | yes | yes | yes |
    // | R3 | yes | no  | no  |
    assert!(local_binding_update_admitted(false, false), "R1/E1");
    assert!(local_binding_update_admitted(true, true), "R2/E1");
    assert!(!local_binding_update_admitted(true, false), "R3/E2");
}

#[test]
fn local_binding_batch_admission_composes_before_publication() {
    // Causes: C1 every member is admitted; C2 a conflict occurs at the first,
    // middle, or last position. Effects: E1 only C1 permits the complete-map
    // publication value; E2 every C2 position rejects before that value exists.
    // The production stage borrows the current map immutably and returns an
    // owned replacement, so the subsequent assignment has no prefix state.
    //
    // | Rule | members | publication permitted |
    // |---|---|---|
    // | R1 | yes, yes, yes | yes |
    // | R2 | no, yes, yes | no |
    // | R3 | yes, no, yes | no |
    // | R4 | yes, yes, no | no |
    for (rule, members, expected) in [
        ("R1/E1", [true, true, true], true),
        ("R2/E2", [false, true, true], false),
        ("R3/E2", [true, false, true], false),
        ("R4/E2", [true, true, false], false),
    ] {
        let admission = members
            .into_iter()
            .fold(LocalBindingBatchAdmission::empty(), |batch, member| {
                batch.include(member)
            });
        assert_eq!(admission.permits_publication(), expected, "{rule}");
    }
}

#[test]
fn wrapper_cache_requires_the_exact_pinned_package_manifest() {
    let directory = tempfile::tempdir().unwrap();
    let manifest = directory.path().join("package.json");
    std::fs::write(
        &manifest,
        r#"{"dependencies":{"@agentclientprotocol/claude-agent-acp":"0.69.0"}}"#,
    )
    .unwrap();
    assert!(installed_wrapper_matches(
        directory.path(),
        "@agentclientprotocol/claude-agent-acp@0.69.0"
    ));
    assert!(
        !installed_wrapper_matches(
            directory.path(),
            "@agentclientprotocol/claude-agent-acp@0.64.2"
        ),
        "a catalog downgrade or upgrade cannot reuse different bytes"
    );

    std::fs::write(&manifest, b"not-json").unwrap();
    assert!(
        !installed_wrapper_matches(
            directory.path(),
            "@agentclientprotocol/claude-agent-acp@0.69.0"
        ),
        "a corrupt manifest fails closed"
    );
    std::fs::remove_file(&manifest).unwrap();
    assert!(
        !installed_wrapper_matches(
            directory.path(),
            "@agentclientprotocol/claude-agent-acp@0.69.0"
        ),
        "a legacy unversioned cache must be reacquired once"
    );
    assert!(!installed_wrapper_matches(directory.path(), "unversioned"));
}

#[tokio::test]
async fn direct_host_acquisition_pins_the_probe_executable_for_worker_launch() {
    // Causes: C1 a direct catalog executable exists on the preparation PATH;
    // C2 it is absent; C3 the catalog carries an ACP flag. Effects: E1 the
    // Worker argv starts with the canonical absolute executable and retains
    // C3, E2 missing executable refuses preparation. The existing wrapper
    // cache test owns pinned npm acquisition; neither branch adds a PATH
    // lookup after the Worker has entered its restricted sandbox.
    // | Rule | C1 | C2 | C3 | Effect |
    // | D1 | yes | no | yes | E1 |
    // | D2 | no | yes | any | E2 |
    let directory = tempfile::tempdir().unwrap();
    let fixture = directory.path().join("gemini");
    std::fs::write(&fixture, b"fixture").unwrap();
    let found = capability_probe::executable_on_path("gemini", Some(directory.path().as_os_str()));
    assert_eq!(found, fixture, "D1 finds the acquisition PATH entry");
    assert_eq!(
        canonical_launch_argv(&found, &["--acp"]).unwrap(),
        vec![
            fixture
                .canonicalize()
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            "--acp".into()
        ],
        "D1 keeps the exact executable and flag",
    );
    let missing =
        capability_probe::executable_on_path("missing", Some(directory.path().as_os_str()));
    assert!(canonical_launch_argv(&missing, &[]).is_err(), "D2");

    let current = std::env::current_exe().unwrap().canonicalize().unwrap();
    let executable: &'static str =
        Box::leak(current.to_string_lossy().into_owned().into_boxed_str());
    let mut cli = *acp_cli("gemini").unwrap();
    cli.acquisition = crate::AcpAcquisition::Direct {
        executable,
        args: &["--acp"],
    };
    let argv = NpmWrapperInstaller
        .resolved_argv(&cli, directory.path())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        argv,
        vec![executable.to_string(), "--acp".into()],
        "D1 production adapter"
    );
}

struct AvailableDiscovery;

fn observation(
    id: &str,
    detection: AcpDetectionState,
    credential: Option<CredentialObservationState>,
    capability: Option<AcpCapabilityObservationState>,
) -> AcpHostObservation {
    AcpHostObservation {
        cli_id: id.into(),
        display_name: id.into(),
        detection,
        version: None,
        credential_state: credential,
        reason_code: None,
        capability_state: capability,
        capability_fingerprint: None,
        capability_reason_code: None,
    }
}

#[async_trait]
impl AcpDiscovery for AvailableDiscovery {
    async fn discover(&self, cli: &AcpCli) -> AcpHostObservation {
        AcpHostObservation {
            cli_id: cli.id.into(),
            display_name: cli.display_name.into(),
            detection: AcpDetectionState::Detected,
            version: Some("test".into()),
            credential_state: Some(CredentialObservationState::Available),
            reason_code: None,
            capability_state: None,
            capability_fingerprint: None,
            capability_reason_code: None,
        }
    }
}

#[tokio::test]
async fn host_credential_refresh_uses_its_injected_clock_for_each_observation() {
    // Causes: C1 one enabled WorkerLocal binding has an available Host CLI;
    // C2 the composing Worker clock reads 1,000 then 2,000 across refreshes.
    // Effects: R1 C1+C2 returns the same credential and Available state with
    // each exact observed_at_ms; no resolver-local wall clock or stale cached
    // timestamp can strengthen the later heartbeat.
    let clock = Arc::new(ManualClock::new(1_000));
    let resolver = AcpLocalCredentialResolver::from_bindings(
        Arc::new(AvailableDiscovery),
        [AcpLocalCredentialBinding {
            credential: CredentialRef {
                id: "ref-clock".into(),
                revision: 1,
            },
            cli_id: "codex".into(),
            enabled: true,
        }],
        clock.clone(),
    )
    .unwrap();
    let first = resolver.credential_observations().await.unwrap();
    let first = first.iter().next().expect("R1 first observation");
    assert_eq!(first.observed_at_ms, 1_000, "R1 first stamp");
    assert_eq!(first.state, CredentialObservationState::Available, "R1");
    clock.set(2_000);
    let later = resolver.credential_observations().await.unwrap();
    let later = later.iter().next().expect("R1 later observation");
    assert_eq!(later.observed_at_ms, 2_000, "R1 later stamp");
    assert_eq!(later.credential, first.credential, "R1 exact binding");
}

#[test]
fn worker_route_admission_follows_the_capability_decision_table() {
    // Causes: catalog selection, executable detection, current login, and
    // successful protocol negotiation. Acquisition argv is diagnostic
    // evidence until the same route decision admits its CLI. Effects:
    // R1 unselected -> no route; R2 missing -> no route;
    // R3 detected + not logged in -> route for live revalidation;
    // R4 available + negotiation failed -> no route and no launch argv;
    // R5 available + verified -> route and its launch argv.
    let selected = ["codex".to_string()].into_iter().collect();
    assert!(
        routable_cli_ids(
            &[observation(
                "claude",
                AcpDetectionState::Detected,
                None,
                None
            )],
            &selected
        )
        .is_empty(),
        "R1"
    );
    assert!(
        routable_cli_ids(
            &[observation("codex", AcpDetectionState::Missing, None, None)],
            &selected
        )
        .is_empty(),
        "R2"
    );
    assert_eq!(
        routable_cli_ids(
            &[observation(
                "codex",
                AcpDetectionState::Detected,
                Some(CredentialObservationState::LoginRequired),
                Some(AcpCapabilityObservationState::Unavailable),
            )],
            &selected,
        ),
        ["codex"],
        "R3"
    );
    assert!(
        routable_cli_ids(
            &[observation(
                "codex",
                AcpDetectionState::Detected,
                Some(CredentialObservationState::Available),
                Some(AcpCapabilityObservationState::ProbeFailed),
            )],
            &selected,
        )
        .is_empty(),
        "R4"
    );
    assert_eq!(
        routable_cli_ids(
            &[observation(
                "codex",
                AcpDetectionState::Detected,
                Some(CredentialObservationState::Available),
                Some(AcpCapabilityObservationState::Verified),
            )],
            &selected,
        ),
        ["codex"],
        "R5"
    );

    let discovery: Arc<dyn AcpDiscovery> = Arc::new(AvailableDiscovery);
    let resolver = Arc::new(
        AcpLocalCredentialResolver::from_bindings(discovery, [], awaken_time::system_clock())
            .expect("empty resolver"),
    );
    for (rule, capability, expected) in [
        (
            "R4",
            AcpCapabilityObservationState::ProbeFailed,
            BTreeMap::new(),
        ),
        (
            "R5",
            AcpCapabilityObservationState::Verified,
            BTreeMap::from([("codex".to_string(), vec!["/wrapper/codex".to_string()])]),
        ),
    ] {
        let prepared = PreparedHostAcpCapabilities {
            observations: vec![observation(
                "codex",
                AcpDetectionState::Detected,
                Some(CredentialObservationState::Available),
                Some(capability),
            )],
            launch_argv: BTreeMap::from([(
                "codex".to_string(),
                vec!["/wrapper/codex".to_string()],
            )]),
            effective_profiles: BTreeMap::new(),
            resolver: resolver.clone(),
            selected_cli_ids: ["codex".to_string()].into_iter().collect(),
        };
        assert_eq!(prepared.routable_launch_argv(), expected, "{rule}");
    }
}
