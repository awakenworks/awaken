// @formal entrypoint worker.acp_capability_is_detected_only_after_a_verified_handshake
use std::sync::Arc;

use awaken_time::WallClockExt as _;

/// Project the one executable ACP catalog into the management read model. This
/// startup edge is intentionally the only place that knows both contexts;
/// neither Control nor the executor keeps a synchronized adapter list.
fn runtime_capabilities(
    observations: &[awaken_acp_application::AcpHostObservation],
) -> Vec<awaken_control::RuntimeCapability> {
    std::iter::once(awaken_control::RuntimeCapability::native())
        .chain(awaken_run_executor_acp::known_acp_clis().iter().map(|cli| {
            let mut capability =
                awaken_control::RuntimeCapability::acp(cli.id, cli.display_name, cli.description);
            capability.supports_tool_aliases = cli.supports_tool_aliases;
            let Some(observation) = observations.iter().find(|row| row.cli_id == cli.id) else {
                return capability;
            };
            capability.with_local(awaken_control::LocalRuntimeCapability {
                detected: observation.detected(),
                version: observation.version.clone(),
                login_state: observation.credential_state.map(credential_state_name),
                reason_code: observation.reason_code.clone(),
                remediation: cli
                    .remediation(observation.reason_code.as_deref())
                    .map(str::to_string),
                negotiated: None,
            })
        }))
        .collect()
}

fn credential_state_name(state: awaken_credential_contract::CredentialObservationState) -> String {
    serde_json::to_value(state)
        .expect("credential observation state serializes")
        .as_str()
        .expect("credential observation state serializes as a string")
        .to_string()
}

fn unavailable_local(reason_code: &str) -> awaken_control::LocalRuntimeCapability {
    awaken_control::LocalRuntimeCapability {
        detected: false,
        version: None,
        login_state: Some("unavailable".into()),
        reason_code: Some(reason_code.into()),
        remediation: None,
        negotiated: None,
    }
}

/// Either live Worker evidence decides this row, or there is none of it. The
/// absence of live evidence is not itself a diagnosis: startup host discovery
/// already knows *why* a local runtime cannot run (no executable, incompatible
/// version, unusable login, failed wrapper install, failed ACP handshake), and
/// collapsing every one of those physical causes into `acp_runtime_unavailable`
/// leaves the published capability row unactionable.
enum LiveLocalEvidence {
    Decided(awaken_control::LocalRuntimeCapability),
    Absent,
}

/// Resolve the published local row. Live evidence always wins; the startup
/// diagnosis only fills the silence left when there is none. `detected` is
/// never restored from startup, because only a verified live handshake may
/// claim an executable route.
fn resolve_local(
    live: LiveLocalEvidence,
    observation: Option<&awaken_acp_application::AcpHostObservation>,
    discovered: Option<&awaken_control::LocalRuntimeCapability>,
) -> awaken_control::LocalRuntimeCapability {
    match (live, observation, discovered) {
        (LiveLocalEvidence::Decided(local), _, _) => local,
        (LiveLocalEvidence::Absent, Some(observation), Some(discovered)) => {
            startup_diagnosis(observation, discovered)
                .unwrap_or_else(|| unavailable_local("acp_runtime_unavailable"))
        }
        (LiveLocalEvidence::Absent, _, _) => unavailable_local("acp_runtime_unavailable"),
    }
}

/// The startup row explains a missing live route exactly when host discovery
/// itself refused the runtime, or when its ACP handshake failed. A healthy
/// startup row explains nothing about the absence of a Worker route, so it must
/// not mask `acp_runtime_unavailable`.
fn startup_diagnosis(
    observation: &awaken_acp_application::AcpHostObservation,
    discovered: &awaken_control::LocalRuntimeCapability,
) -> Option<awaken_control::LocalRuntimeCapability> {
    let refused = !observation.detected()
        || observation.credential_state
            != Some(awaken_credential_contract::CredentialObservationState::Available);
    if refused {
        return Some(awaken_control::LocalRuntimeCapability {
            detected: false,
            negotiated: None,
            ..discovered.clone()
        });
    }
    if observation.capability_state == Some(awaken_acp_application::AcpCapabilityState::ProbeFailed)
    {
        // Discovery and login both succeeded; the backend handshake did not.
        // That is a different physical cause from an absent runtime and keeps
        // the observed version and login evidence beside it.
        return Some(awaken_control::LocalRuntimeCapability {
            detected: false,
            reason_code: observation.capability_reason_code.clone(),
            // The catalog owns remediation per discovery reason; a failed
            // handshake has none beyond re-running discovery.
            remediation: None,
            negotiated: None,
            ..discovered.clone()
        });
    }
    None
}

fn live_acp_local(
    backend_ref: &str,
    workers: &[awaken_worker_contract::RegisteredWorker],
    credentials: &[awaken_credential_vault::CredentialSource],
    now_ms: u64,
) -> LiveLocalEvidence {
    let matching = credentials.iter().filter(|source| {
        source.status == awaken_credential_vault::CredentialStatus::Active
            && source.kind == awaken_credential_vault::CredentialKind::WorkerLocal
            && source
                .worker_local_binding
                .as_ref()
                .is_some_and(|binding| binding.driver_id == backend_ref)
    });
    let matching = matching.collect::<Vec<_>>();
    // Protocol availability is independent of any ambient WorkerLocal source.
    // A Provider-owned publication supplies its own credential later; the
    // credential-bound selector below proves only backend-owned login readiness.
    let protocol = match awaken_worker_contract::select_verified_acp_capability(
        workers,
        backend_ref,
        None,
        now_ms,
    ) {
        awaken_worker_contract::VerifiedAcpCapabilitySelection::Available(only) => only,
        awaken_worker_contract::VerifiedAcpCapabilitySelection::Unavailable => {
            return LiveLocalEvidence::Absent;
        }
        awaken_worker_contract::VerifiedAcpCapabilitySelection::Incompatible => {
            return LiveLocalEvidence::Decided(unavailable_local("acp_capability_ambiguous"));
        }
    };
    let has_local_source = !matching.is_empty();
    let mut evidence = Vec::new();
    for source in matching {
        let Some(revision) = u64::try_from(source.version)
            .ok()
            .filter(|revision| *revision > 0)
        else {
            continue;
        };
        let credential = awaken_worker_contract::WorkerCredentialRevision {
            id: source.id.0.clone(),
            revision,
        };
        match awaken_worker_contract::select_verified_acp_capability(
            workers,
            backend_ref,
            Some(&credential),
            now_ms,
        ) {
            awaken_worker_contract::VerifiedAcpCapabilitySelection::Available(candidate) => {
                if !evidence.contains(&candidate) {
                    evidence.push(candidate);
                }
            }
            awaken_worker_contract::VerifiedAcpCapabilitySelection::Incompatible => {
                return LiveLocalEvidence::Decided(unavailable_local("acp_capability_ambiguous"));
            }
            awaken_worker_contract::VerifiedAcpCapabilitySelection::Unavailable => {}
        }
    }
    let login_state = match evidence.as_slice() {
        [_] => Some("available".into()),
        [] if has_local_source => Some("unavailable".into()),
        [] => None,
        _ => return LiveLocalEvidence::Decided(unavailable_local("acp_capability_ambiguous")),
    };
    LiveLocalEvidence::Decided(awaken_control::LocalRuntimeCapability {
        detected: true,
        version: Some(protocol.adapter_version),
        login_state,
        reason_code: None,
        remediation: None,
        negotiated: serde_json::to_value(protocol.negotiated).ok(),
    })
}

pub(crate) struct LiveRuntimeCapabilities {
    pub(crate) initial: Vec<awaken_acp_application::AcpHostObservation>,
    pub(crate) workers: Arc<dyn awaken_coordinator::WorkerObservationSource>,
    pub(crate) credentials: Arc<dyn awaken_credential_vault::repo::CredentialRepo>,
    pub(crate) workspace: String,
    pub(crate) clock: Arc<dyn awaken_time::WallClock>,
}

#[async_trait::async_trait]
impl awaken_control::RuntimeCapabilitySource for LiveRuntimeCapabilities {
    async fn current(&self) -> Vec<awaken_control::RuntimeCapability> {
        let now = self.clock.now_ms();
        let workers = self.workers.list().await;
        let credentials = self.credentials.list(&self.workspace).await;
        runtime_capabilities(&self.initial)
            .into_iter()
            .map(|mut capability| {
                if capability.cli.is_none() {
                    return capability;
                }
                let backend_ref = capability.id.clone();
                let observation = capability
                    .cli
                    .as_deref()
                    .and_then(|cli_id| self.initial.iter().find(|row| row.cli_id == cli_id));
                let local = match (&workers, &credentials) {
                    (Err(_), _) => unavailable_local("worker_observations_unavailable"),
                    (_, Err(_)) => unavailable_local("credential_observations_unavailable"),
                    (Ok(workers), Ok(credentials)) => resolve_local(
                        live_acp_local(&backend_ref, workers, credentials, now),
                        observation,
                        capability.local.as_ref(),
                    ),
                };
                capability = capability.with_local(local);
                capability
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credential(
        id: &str,
        backend_ref: &str,
        status: awaken_credential_vault::CredentialStatus,
    ) -> awaken_credential_vault::CredentialSource {
        awaken_credential_vault::CredentialSource {
            id: awaken_credential_contract::CredentialSourceId(id.into()),
            replacement_of: None,
            workspace_id: "workspace".into(),
            kind: awaken_credential_vault::CredentialKind::WorkerLocal,
            descriptor: None,
            provider_id: None,
            protocol_endpoint_id: None,
            env_key: None,
            material_ref: None,
            auxiliary_material_refs: Default::default(),
            oauth_command: None,
            worker_local_binding: Some(awaken_credential_vault::WorkerLocalBinding::new(
                backend_ref,
                "local-user",
            )),
            status,
            version: 1,
        }
    }

    fn worker(
        id: &str,
        credential_id: Option<&str>,
        fingerprint: Option<&str>,
    ) -> awaken_worker_contract::RegisteredWorker {
        let now = 100;
        let credential_observations = credential_id
            .map(|id| {
                [
                    awaken_worker_contract::WorkerCredentialObservation::available(
                        awaken_worker_contract::WorkerCredentialRevision {
                            id: id.into(),
                            revision: 1,
                        },
                        now,
                        200,
                    ),
                ]
                .into_iter()
                .collect()
            })
            .unwrap_or_default();
        let acp_capability_observations = fingerprint
            .map(|fingerprint| {
                vec![awaken_worker_contract::WorkerAcpCapabilityObservation {
                    observation: awaken_acp_contract::AcpCapabilityObservation::verified(
                        "acp:claude",
                        "1",
                        now,
                        fingerprint,
                        awaken_acp_contract::NegotiatedAcpCapabilities {
                            protocol_version: "1".into(),
                            load_session: false,
                            prompt_image: false,
                            prompt_audio: false,
                            prompt_embedded_context: false,
                            mcp_http: true,
                            mcp_sse: false,
                            session_list: false,
                            modes: Vec::new(),
                            config_options: Vec::new(),
                        },
                    )
                    .unwrap(),
                    valid_until_ms: 200,
                }]
            })
            .unwrap_or_default();
        awaken_worker_contract::RegisteredWorker {
            snapshot: awaken_worker_contract::WorkerSnapshot {
                identity: awaken_worker_contract::WorkerIdentity::new(id, format!("boot-{id}"), 1),
                state: awaken_worker_contract::WorkerState::Ready,
                manifest: awaken_worker_contract::WorkerManifest::default(),
                capability_fingerprint: "manifest".into(),
                in_flight: 0,
                warm_environment_shapes: Default::default(),
                credential_observations,
                acp_capability_observations,
                expires_at_ms: 200,
            },
            heartbeat_sequence: 1,
            observation_sequence: 1,
            registered_at_ms: 1,
            heartbeat_at_ms: now,
            drain_deadline_ms: None,
            process_exit: None,
            create_ledger: None,
            source_retention: Default::default(),
        }
    }

    struct FixedWorkers(Vec<awaken_worker_contract::RegisteredWorker>);

    #[async_trait::async_trait]
    impl awaken_coordinator::WorkerObservationSource for FixedWorkers {
        async fn list(
            &self,
        ) -> Result<
            Vec<awaken_worker_contract::RegisteredWorker>,
            awaken_worker_contract::RegistryError,
        > {
            Ok(self.0.clone())
        }
    }

    #[tokio::test]
    async fn management_runtime_query_judges_evidence_on_its_injected_clock() {
        use awaken_control::RuntimeCapabilitySource as _;

        // Causes: C1 the clock precedes a Worker's observed capability, C2 it
        // is inside both the Worker and capability validity windows, C3 it
        // reaches their common exclusive expiry. The Worker and credential
        // snapshots stay identical across calls. Effects: E1/E3 unavailable,
        // E2 detected. Decision rules: R1 99 -> E1, R2 150 -> E2,
        // R3 200 -> E3; changing only the injected clock changes the verdict.
        let clock = Arc::new(awaken_lease_testkit::ManualClock::new(99));
        let source = LiveRuntimeCapabilities {
            initial: Vec::new(),
            workers: Arc::new(FixedWorkers(vec![worker(
                "current",
                None,
                Some("sha256:capability"),
            )])),
            credentials: Arc::new(awaken_credential_vault::repo::InMemoryCredentialRepo::new()),
            workspace: "workspace".into(),
            clock: clock.clone(),
        };
        let detected = |rows: Vec<awaken_control::RuntimeCapability>| {
            rows.into_iter()
                .find(|row| row.id == "acp:claude")
                .expect("known Claude ACP capability")
                .local
                .expect("current() always projects local evidence")
                .detected
        };

        assert!(!detected(source.current().await), "R1 before observation");
        clock.set(150);
        assert!(detected(source.current().await), "R2 live observation");
        clock.set(200);
        assert!(!detected(source.current().await), "R3 expired observation");
    }

    #[test]
    fn management_runtime_projection_is_exactly_the_executable_catalog() {
        // Cause graph:
        // C1 native runtime is intrinsic -> E1 exactly one `awaken` row.
        // C2 an AcpCli catalog row exists -> E2 exactly one matching `acp:<id>` row.
        // C3 no AcpCli row exists -> E3 no management capability can advertise it.
        //
        // Decision table:
        // | Rule | Native | catalog row | capability |
        // | R1 | T | - | awaken exactly once |
        // | R2 | - | T | matching acp:<id> exactly once |
        // | R3 | - | F | absent |
        let projected = runtime_capabilities(&[]);
        assert_eq!(
            projected.iter().filter(|row| row.id == "awaken").count(),
            1,
            "R1"
        );

        let catalog = awaken_run_executor_acp::known_acp_clis();
        let projected_acp: Vec<_> = projected.iter().filter(|row| row.kind == "acp").collect();
        assert_eq!(projected_acp.len(), catalog.len(), "R2/R3 cardinality");
        for cli in catalog {
            let row = projected_acp
                .iter()
                .find(|row| row.cli.as_deref() == Some(cli.id))
                .unwrap_or_else(|| panic!("R2 missing catalog projection for {}", cli.id));
            assert_eq!(row.id, format!("acp:{}", cli.id), "R2");
            assert_eq!(row.label, cli.display_name, "R2 metadata");
            assert_eq!(row.description, cli.description, "R2 metadata");
        }
    }

    #[test]
    fn management_runtime_projection_joins_one_secret_free_local_observation() {
        // Cause graph: catalog row + same-id startup observation -> enriched
        // read model. Rows without an observation stay supported with unknown
        // local status; no joined inventory is persisted.
        //
        // Decision table:
        // L1 same id + detected/login-required -> detected status + remediation
        // L2 no observation                    -> local status absent
        let projected = runtime_capabilities(&[awaken_acp_application::AcpHostObservation {
            cli_id: "codex".into(),
            display_name: "Codex".into(),
            detection: awaken_acp_application::AcpDetectionState::Detected,
            version: Some("codex 1".into()),
            credential_state: Some(
                awaken_credential_contract::CredentialObservationState::LoginRequired,
            ),
            reason_code: Some("acp_login_required".into()),
            capability_state: None,
            capability_fingerprint: None,
            capability_reason_code: None,
        }]);
        let codex = projected.iter().find(|row| row.id == "acp:codex").unwrap();
        let local = codex.local.as_ref().expect("L1");
        assert!(local.detected, "L1");
        assert_eq!(local.login_state.as_deref(), Some("login_required"), "L1");
        assert!(
            local
                .remediation
                .as_deref()
                .unwrap()
                .contains("codex login")
        );
        assert!(
            projected
                .iter()
                .find(|row| row.id == "acp:gemini")
                .unwrap()
                .local
                .is_none(),
            "L2"
        );
    }

    #[test]
    fn worker_manifest_never_substitutes_for_a_verified_acp_handshake() {
        use awaken_acp_contract::AcpCapabilityObservationState;

        // Cause/effect graph: a manifest only declares an intended route. A
        // fresh coherent handshake is the sole cause of `detected`; absence or
        // probe failure must remain unavailable and retain its diagnostic.
        //
        // Decision table:
        // | Rule | live manifest | observation | detected |
        // | V1 | yes | missing | false |
        // | V2 | yes | probe_failed | false |
        // | V3 | yes | verified | true |
        assert!(
            !awaken_acp_contract::has_verified_capability_observation(None),
            "V1"
        );
        assert!(
            !awaken_acp_contract::has_verified_capability_observation(Some(
                AcpCapabilityObservationState::ProbeFailed
            )),
            "V2"
        );
        assert!(
            awaken_acp_contract::has_verified_capability_observation(Some(
                AcpCapabilityObservationState::Verified
            )),
            "V3"
        );
    }

    /// R1-R5 below concern live evidence alone. With no startup observation to
    /// consult, the published row is exactly the live decision.
    fn live_local(
        backend_ref: &str,
        workers: &[awaken_worker_contract::RegisteredWorker],
        credentials: &[awaken_credential_vault::CredentialSource],
        now_ms: u64,
    ) -> awaken_control::LocalRuntimeCapability {
        resolve_local(
            live_acp_local(backend_ref, workers, credentials, now_ms),
            None,
            None,
        )
    }

    #[test]
    fn management_readiness_requires_coherent_worker_credential_and_capability_evidence() {
        // Cause/effect graph: C1 an active Worker-local source exists; C2 its
        // exact revision and a verified ACP profile are live on one Ready
        // Worker; C3 all qualifying profiles agree; C4 no Worker-local source
        // exists because the publication will deliver Provider/workload
        // material later. Effects: E1 protocol and local login are available; E2
        // split/missing credential evidence cannot claim login; E3 incompatible profiles fail
        // closed; E4 capability-only discovery reports no local-login claim.
        //
        // | Rule | local source | same-worker credential | ACP agreement | Effect |
        // | R1 | yes | yes | one | E1 |
        // | R2 | yes | no | one | protocol detected, login unavailable (E2) |
        // | R3 | any | allowed | conflict | E3 |
        // | R4 | no | n/a | one | E1,E4 |
        // | R5 | any | any | missing/expired | protocol unavailable |
        //
        // This management projection reuses the publication selector. R4 only
        // proves protocol capability; publication still requires its exact
        // credential when the selected model binding names one. R2 keeps that
        // same-worker credential boundary without hiding protocol capability
        // from a Provider-owned publication that does not use the local source.
        let active = credential(
            "credential-1",
            "acp:claude",
            awaken_credential_vault::CredentialStatus::Active,
        );
        let ready = live_local(
            "acp:claude",
            &[worker("ready", Some("credential-1"), Some("sha256:same"))],
            std::slice::from_ref(&active),
            150,
        );
        assert!(ready.detected, "R1");
        assert_eq!(ready.login_state.as_deref(), Some("available"), "R1");

        let split = live_local(
            "acp:claude",
            &[
                worker("credential", Some("credential-1"), None),
                worker("capability", None, Some("sha256:same")),
            ],
            std::slice::from_ref(&active),
            150,
        );
        assert!(split.detected, "R2 protocol is independently verified");
        assert_eq!(split.login_state.as_deref(), Some("unavailable"), "R2");
        assert!(
            split.negotiated.is_some(),
            "R2 Provider-owned route remains visible"
        );
        let protocol_only = worker("protocol-only", None, Some("sha256:same"));
        let no_login = live_local(
            "acp:claude",
            std::slice::from_ref(&protocol_only),
            std::slice::from_ref(&active),
            150,
        );
        assert!(
            no_login.detected,
            "R2 unrelated local source cannot hide protocol"
        );
        assert_eq!(no_login.login_state.as_deref(), Some("unavailable"), "R2");
        let expired = live_local(
            "acp:claude",
            &[protocol_only],
            std::slice::from_ref(&active),
            201,
        );
        assert!(
            !expired.detected,
            "R5 expired protocol never becomes usable"
        );

        let incompatible = live_local(
            "acp:claude",
            &[
                worker("first", Some("credential-1"), Some("sha256:first")),
                worker("second", Some("credential-1"), Some("sha256:second")),
            ],
            &[active],
            150,
        );
        assert!(!incompatible.detected, "R3");
        assert_eq!(
            incompatible.reason_code.as_deref(),
            Some("acp_capability_ambiguous"),
            "R3"
        );

        let provider_delivered = live_local(
            "acp:claude",
            &[worker("provider", None, Some("sha256:same"))],
            &[],
            150,
        );
        assert!(provider_delivered.detected, "R4");
        assert_eq!(provider_delivered.login_state, None, "R4/E4");
        assert_eq!(provider_delivered.reason_code, None, "R4/E4");

        let absent = live_local("acp:claude", &[worker("absent", None, None)], &[], 150);
        assert!(!absent.detected, "R5");
        assert_eq!(
            absent.reason_code.as_deref(),
            Some("acp_runtime_unavailable"),
            "R5"
        );
    }

    fn observed(
        detection: awaken_acp_application::AcpDetectionState,
        credential_state: Option<awaken_credential_contract::CredentialObservationState>,
        reason_code: &str,
    ) -> awaken_acp_application::AcpHostObservation {
        awaken_acp_application::AcpHostObservation {
            cli_id: "codex".into(),
            display_name: "Codex".into(),
            detection,
            version: Some("codex 9.9.9".into()),
            credential_state,
            reason_code: Some(reason_code.into()),
            capability_state: None,
            capability_fingerprint: None,
            capability_reason_code: None,
        }
    }

    /// Project one startup observation through the exact management edge used
    /// in production when no Worker has published live ACP evidence.
    fn published(
        observation: &awaken_acp_application::AcpHostObservation,
    ) -> awaken_control::LocalRuntimeCapability {
        let capabilities = runtime_capabilities(std::slice::from_ref(observation));
        let capability = capabilities
            .iter()
            .find(|row| row.id == "acp:codex")
            .expect("the executable catalog publishes codex");
        resolve_local(
            LiveLocalEvidence::Absent,
            Some(observation),
            capability.local.as_ref(),
        )
    }

    #[test]
    fn every_physical_discovery_cause_publishes_its_own_capability_reason_code() {
        use awaken_acp_application::AcpDetectionState;
        use awaken_credential_contract::CredentialObservationState;

        // Without live Worker evidence the capability row is the *only* place an
        // operator learns why a local runtime cannot run. Each physical cause
        // therefore keeps its own code; `acp_runtime_unavailable` names exactly
        // one cause — a healthy host with no published route — and never stands
        // in for a startup refusal.
        //
        // Decision table (physical condition -> published login_state/reason_code):
        // | Rule | physical condition                  | login_state   | reason_code |
        // | T1 | CLI executable absent                  | none          | acp_agent_missing |
        // | T2 | version command failed/timed out       | none          | acp_version_probe_failed |
        // | T3 | version below the catalog minimum      | none          | acp_version_unsupported |
        // | T4 | login command could not be run         | probe_failed  | acp_login_probe_failed |
        // | T5 | login output matched no catalog rule   | probe_failed  | acp_login_probe_unrecognized |
        // | T6 | CLI healthy, not logged in             | login_required| acp_login_required |
        // | T7 | wrapper installer missing/failed       | probe_failed  | acp_wrapper_install_failed |
        // | T8 | wrapper present, ACP handshake failed  | available     | acp_capability_probe_failed |
        // | T9 | host healthy, no live route published  | unavailable   | acp_runtime_unavailable |
        let mut handshake_failed = observed(
            AcpDetectionState::Detected,
            Some(CredentialObservationState::Available),
            "acp_login_available",
        );
        handshake_failed.capability_state =
            Some(awaken_acp_application::AcpCapabilityState::ProbeFailed);
        handshake_failed.capability_reason_code = Some("acp_capability_probe_failed".into());
        let mut healthy = observed(
            AcpDetectionState::Detected,
            Some(CredentialObservationState::Available),
            "acp_login_available",
        );
        healthy.capability_state = Some(awaken_acp_application::AcpCapabilityState::Verified);

        let table = [
            (
                "T1",
                observed(AcpDetectionState::Missing, None, "acp_agent_missing"),
                None,
                "acp_agent_missing",
            ),
            (
                "T2",
                observed(
                    AcpDetectionState::ProbeFailed,
                    None,
                    "acp_version_probe_failed",
                ),
                None,
                "acp_version_probe_failed",
            ),
            (
                "T3",
                observed(
                    AcpDetectionState::ProbeFailed,
                    None,
                    "acp_version_unsupported",
                ),
                None,
                "acp_version_unsupported",
            ),
            (
                "T4",
                observed(
                    AcpDetectionState::Detected,
                    Some(CredentialObservationState::ProbeFailed),
                    "acp_login_probe_failed",
                ),
                Some("probe_failed"),
                "acp_login_probe_failed",
            ),
            (
                "T5",
                observed(
                    AcpDetectionState::Detected,
                    Some(CredentialObservationState::ProbeFailed),
                    "acp_login_probe_unrecognized",
                ),
                Some("probe_failed"),
                "acp_login_probe_unrecognized",
            ),
            (
                "T6",
                observed(
                    AcpDetectionState::Detected,
                    Some(CredentialObservationState::LoginRequired),
                    "acp_login_required",
                ),
                Some("login_required"),
                "acp_login_required",
            ),
            (
                "T7",
                observed(
                    AcpDetectionState::ProbeFailed,
                    Some(CredentialObservationState::ProbeFailed),
                    "acp_wrapper_install_failed",
                ),
                Some("probe_failed"),
                "acp_wrapper_install_failed",
            ),
            (
                "T8",
                handshake_failed,
                Some("available"),
                "acp_capability_probe_failed",
            ),
            (
                "T9",
                healthy,
                Some("unavailable"),
                "acp_runtime_unavailable",
            ),
        ];

        let mut codes = std::collections::BTreeSet::new();
        for (rule, observation, login_state, reason_code) in &table {
            let local = published(observation);
            assert!(
                !local.detected,
                "{rule}: no live evidence can claim a route"
            );
            assert_eq!(local.login_state.as_deref(), *login_state, "{rule}");
            assert_eq!(local.reason_code.as_deref(), Some(*reason_code), "{rule}");
            codes.insert(*reason_code);
        }
        // Mutation probe: conflating any two physical conditions onto one code
        // collapses this set, so the count is what fails first.
        assert_eq!(
            codes.len(),
            table.len(),
            "each physical condition owns a distinct reason code"
        );

        // The catalog-owned remediation reaches the capability row, not only
        // `doctor acp`; a conflated code silently drops it.
        assert!(
            published(&observed(
                AcpDetectionState::Missing,
                None,
                "acp_agent_missing"
            ))
            .remediation
            .is_some(),
            "T1 remediation"
        );
        assert!(
            published(&observed(
                AcpDetectionState::Detected,
                Some(CredentialObservationState::LoginRequired),
                "acp_login_required",
            ))
            .remediation
            .is_some(),
            "T6 remediation"
        );
    }
}
