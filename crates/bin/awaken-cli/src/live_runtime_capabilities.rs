use std::sync::Arc;

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

fn credential_state_name(state: awaken_runtime_contract::CredentialObservationState) -> String {
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

fn live_acp_local(
    backend_ref: &str,
    workers: &[awaken_worker_contract::RegisteredWorker],
    credentials: &[awaken_credential_vault::CredentialSource],
    now_ms: u64,
) -> awaken_control::LocalRuntimeCapability {
    let matching = credentials.iter().filter(|source| {
        source.status == awaken_credential_vault::CredentialStatus::Active
            && source.kind == awaken_credential_vault::CredentialKind::WorkerLocal
            && source
                .worker_local_binding
                .as_ref()
                .is_some_and(|binding| binding.driver_id == backend_ref)
    });
    let matching = matching.collect::<Vec<_>>();
    if matching.is_empty() {
        return match awaken_worker_contract::select_verified_acp_capability(
            workers,
            backend_ref,
            None,
            now_ms,
        ) {
            awaken_worker_contract::VerifiedAcpCapabilitySelection::Available(only) => {
                awaken_control::LocalRuntimeCapability {
                    detected: true,
                    version: Some(only.adapter_version),
                    // A capability-only handshake proves the backend protocol,
                    // not a local account. Provider or workload material is
                    // selected later by the immutable Agent publication.
                    login_state: None,
                    reason_code: None,
                    remediation: None,
                    negotiated: serde_json::to_value(only.negotiated).ok(),
                }
            }
            awaken_worker_contract::VerifiedAcpCapabilitySelection::Unavailable => {
                unavailable_local("acp_runtime_unavailable")
            }
            awaken_worker_contract::VerifiedAcpCapabilitySelection::Incompatible => {
                unavailable_local("acp_capability_ambiguous")
            }
        };
    }
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
                return unavailable_local("acp_capability_ambiguous");
            }
            awaken_worker_contract::VerifiedAcpCapabilitySelection::Unavailable => {}
        }
    }
    match evidence.as_slice() {
        [only] => awaken_control::LocalRuntimeCapability {
            detected: true,
            version: Some(only.adapter_version.clone()),
            login_state: Some("available".into()),
            reason_code: None,
            remediation: None,
            negotiated: serde_json::to_value(&only.negotiated).ok(),
        },
        [] => unavailable_local("acp_runtime_unavailable"),
        _ => unavailable_local("acp_capability_ambiguous"),
    }
}

pub(crate) struct LiveRuntimeCapabilities {
    pub(crate) initial: Vec<awaken_acp_application::AcpHostObservation>,
    pub(crate) workers: Arc<dyn awaken_coordinator::WorkerObservationSource>,
    pub(crate) credentials: Arc<dyn awaken_credential_vault::repo::CredentialRepo>,
    pub(crate) workspace: String,
}

#[async_trait::async_trait]
impl awaken_control::RuntimeCapabilitySource for LiveRuntimeCapabilities {
    async fn current(&self) -> Vec<awaken_control::RuntimeCapability> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let workers = self.workers.list().await;
        let credentials = self.credentials.list(&self.workspace).await;
        runtime_capabilities(&self.initial)
            .into_iter()
            .map(|mut capability| {
                if capability.cli.is_none() {
                    return capability;
                }
                let backend_ref = capability.id.clone();
                let local = match (&workers, &credentials) {
                    (Err(_), _) => unavailable_local("worker_observations_unavailable"),
                    (_, Err(_)) => unavailable_local("credential_observations_unavailable"),
                    (Ok(workers), Ok(credentials)) => {
                        live_acp_local(&backend_ref, workers, credentials, now)
                    }
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
        }
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
                awaken_runtime_contract::CredentialObservationState::LoginRequired,
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

    #[test]
    fn management_readiness_requires_coherent_worker_credential_and_capability_evidence() {
        // Cause/effect graph: C1 an active Worker-local source exists; C2 its
        // exact revision and a verified ACP profile are live on one Ready
        // Worker; C3 all qualifying profiles agree; C4 no Worker-local source
        // exists because the publication will deliver Provider/workload
        // material later. Effects: E1 runtime is detected and usable; E2
        // split/missing evidence is unavailable; E3 incompatible profiles fail
        // closed; E4 capability-only discovery reports no local-login claim.
        //
        // | Rule | local source | same-worker credential | ACP agreement | Effect |
        // | R1 | yes | yes | one | E1 |
        // | R2 | yes | no | any | E2 |
        // | R3 | any | allowed | conflict | E3 |
        // | R4 | no | n/a | one | E1,E4 |
        // | R5 | no | n/a | none | E2 |
        //
        // This management projection reuses the publication selector. R4 only
        // proves protocol capability; publication still requires its exact
        // credential when the selected model binding names one.
        let active = credential(
            "credential-1",
            "acp:claude",
            awaken_credential_vault::CredentialStatus::Active,
        );
        let ready = live_acp_local(
            "acp:claude",
            &[worker("ready", Some("credential-1"), Some("sha256:same"))],
            std::slice::from_ref(&active),
            150,
        );
        assert!(ready.detected, "R1");
        assert_eq!(ready.login_state.as_deref(), Some("available"), "R1");

        let split = live_acp_local(
            "acp:claude",
            &[
                worker("credential", Some("credential-1"), None),
                worker("capability", None, Some("sha256:same")),
            ],
            std::slice::from_ref(&active),
            150,
        );
        assert!(!split.detected, "R2");
        assert_eq!(
            split.reason_code.as_deref(),
            Some("acp_runtime_unavailable")
        );

        let incompatible = live_acp_local(
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

        let provider_delivered = live_acp_local(
            "acp:claude",
            &[worker("provider", None, Some("sha256:same"))],
            &[],
            150,
        );
        assert!(provider_delivered.detected, "R4");
        assert_eq!(provider_delivered.login_state, None, "R4/E4");
        assert_eq!(provider_delivered.reason_code, None, "R4/E4");

        let absent = live_acp_local("acp:claude", &[worker("absent", None, None)], &[], 150);
        assert!(!absent.detected, "R5");
        assert_eq!(
            absent.reason_code.as_deref(),
            Some("acp_runtime_unavailable"),
            "R5"
        );
    }
}
