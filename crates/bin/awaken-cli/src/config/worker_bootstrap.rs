use super::Role;

/// Coordinator-owned trust domain. Worker projections carry only their own
/// bound token, never this API-server credential or a mapping authority.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KubernetesWorkerTransport {
    pub namespace: String,
    pub service_account: String,
    pub audience: String,
    pub provider_id: String,
}

impl KubernetesWorkerTransport {
    pub(crate) fn validate(
        &self,
        role: Role,
        mode: super::OperatingMode,
        has_signing_trust: bool,
    ) -> Result<(), String> {
        let owns_coordinator_and_worker =
            role == Role::AllInOne && mode == super::OperatingMode::Local;
        let owns_coordinator = role == Role::Coordinator && mode == super::OperatingMode::Server;
        if !(owns_coordinator || owns_coordinator_and_worker) || has_signing_trust {
            return Err("worker_kubernetes requires a server-mode Coordinator or local AllInOne and cannot be mixed with worker_trust_credentials_file".into());
        }
        if self.provider_id.trim().is_empty() || self.provider_id.trim() != self.provider_id {
            return Err("worker_kubernetes requires an exact nonempty provider_id".into());
        }
        #[cfg(feature = "container-k8s")]
        return awaken_worker_transport_security::kubernetes::KubernetesWorkerAuthenticator::validate_policy(
            &self.namespace, &self.service_account, &self.audience, std::time::Duration::from_secs(5),
        );
        #[cfg(not(feature = "container-k8s"))]
        Err("worker_kubernetes requires the container-k8s build feature".into())
    }
}

pub(super) fn validate_file_ownership(
    role: Role,
    mode: super::OperatingMode,
    file: &super::FileConfig,
) -> Result<(), String> {
    validate_credential_file_ownership(
        role,
        file.worker_request_credential_file.is_some(),
        file.worker_trust_credentials_file.is_some(),
    )?;
    if let Some(policy) = &file.worker_kubernetes {
        policy.validate(role, mode, file.worker_trust_credentials_file.is_some())?;
    }
    if file.worker_kubernetes_identity_directory.is_some()
        && !((role == Role::Worker && mode == super::OperatingMode::Server)
            || (role == Role::AllInOne && mode == super::OperatingMode::Local))
    {
        return Err(
            "worker_kubernetes_identity_directory is owned by a server-mode Worker or local AllInOne"
                .into(),
        );
    }
    if role == Role::Coordinator && file.credential_envelope_keys_file.is_some() {
        return Err(
            "credential_envelope_keys_file belongs to Control or Worker, not Coordinator".into(),
        );
    }
    Ok(())
}

fn validate_credential_file_ownership(
    role: Role,
    request_credential_file: bool,
    trust_credentials_file: bool,
) -> Result<(), String> {
    if !matches!(role, Role::AllInOne | Role::Coordinator) && trust_credentials_file {
        return Err(
            "worker_trust_credentials_file is owned by Coordinator, not this process role"
                .to_owned(),
        );
    }
    if !matches!(role, Role::AllInOne | Role::Worker) && request_credential_file {
        return Err(
            "worker_request_credential_file is owned by Worker, not this process role".to_owned(),
        );
    }
    if role == Role::AllInOne && request_credential_file != trust_credentials_file {
        return Err(
            "AllInOne signed Worker transport requires both worker_request_credential_file and worker_trust_credentials_file"
                .to_owned(),
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_key_file_is_excluded_from_the_coordinator() {
        // E1 no envelope key file => preserve role-owned transport rules.
        // E2 key file + Coordinator => reject before file I/O. E3 Control,
        // Worker or AllInOne => retain the existing issuer/resolver composition.
        for role in [
            Role::Control,
            Role::Coordinator,
            Role::Worker,
            Role::AllInOne,
        ] {
            for keys in [false, true] {
                let file = super::super::FileConfig {
                    credential_envelope_keys_file: keys.then(|| "keys.json".into()),
                    ..Default::default()
                };
                assert_eq!(
                    validate_file_ownership(role, super::super::OperatingMode::Server, &file)
                        .is_ok(),
                    !keys || role != Role::Coordinator,
                    "E1-E3 {role:?} keys={keys}"
                );
            }
        }
    }

    #[test]
    fn kubernetes_file_configuration_reaches_only_its_owning_process() {
        use super::super::{ConfigOverrides, FileConfig, ResolvedDeployment};
        // F1 authored Coordinator policy -> retained unchanged at startup.
        // F2 missing adapter -> explicit rejection, not an unsigned fallback.
        // F3 projected identity on any non-Worker role or Local mode -> reject
        // before reading files. F4 unknown policy fields -> parse rejection.
        // These exercise TOML -> the production resolver, complementing the
        // complete policy decision table below and Worker projection tests.
        let source = r#"
            role = "coordinator"
            mode = "server"
            internal_bind = "127.0.0.1:8081"
            runtime_database_url = "postgres://coordinator/execution"
            control_internal_url = "http://control:3000"
            control_service_token_file = "/run/control-service-token"
            [worker_kubernetes]
            namespace = "workers"
            service_account = "executor"
            audience = "awaken-worker"
            provider_id = "cluster-a"
        "#;
        let resolve = |file| {
            ResolvedDeployment::resolve_file(
                ConfigOverrides::default(),
                Some("/home/dev".into()),
                "/home/dev/config.toml".into(),
                file,
            )
        };
        let result = resolve(toml::from_str::<FileConfig>(source).unwrap());
        if cfg!(feature = "container-k8s") {
            let deployment = result.unwrap();
            let policy = deployment.worker_kubernetes.unwrap();
            assert_eq!(policy.namespace, "workers", "F1");
            assert_eq!(policy.service_account, "executor", "F1");
            assert_eq!(policy.audience, "awaken-worker", "F1");
            assert_eq!(policy.provider_id, "cluster-a", "F1");
            assert!(deployment.worker.kubernetes_identity_directory.is_none());
        } else {
            assert!(result.unwrap_err().contains("container-k8s"), "F2");
        }
        let projection = tempfile::tempdir().unwrap();
        for (name, value) in [
            ("namespace", "workers"),
            ("name", "worker-a"),
            ("uid", "uid-a"),
            ("token", "fixture-token"),
        ] {
            std::fs::write(projection.path().join(name), value).unwrap();
        }
        let all_in_one: FileConfig = toml::from_str(&format!(
            r#"
                role = "all-in-one"
                mode = "local"
                run_local_pool = true
                worker_kubernetes_identity_directory = {projection:?}
                [worker_kubernetes]
                namespace = "workers"
                service_account = "executor"
                audience = "awaken-worker"
                provider_id = "cluster-a"
            "#,
            projection = projection.path().to_str().unwrap()
        ))
        .unwrap();
        let all_in_one = resolve(all_in_one);
        assert_eq!(
            all_in_one.is_ok(),
            cfg!(feature = "container-k8s"),
            "F1b local AllInOne owns both Pod-bound transport ends: {:?}",
            all_in_one.as_ref().err()
        );
        #[cfg(feature = "container-k8s")]
        assert_eq!(
            all_in_one
                .unwrap()
                .worker
                .kubernetes_identity_directory
                .as_deref(),
            Some(projection.path()),
            "F1b"
        );
        for (role, mode) in [
            ("coordinator", "server"),
            ("control", "server"),
            ("all-in-one", "server"),
            ("worker", "local"),
        ] {
            let file: FileConfig = toml::from_str(&format!(
                "role = {role:?}\nmode = {mode:?}\nworker_server = \"https://coordinator\"\nworker_kubernetes_identity_directory = \"/missing-projection\""
            ))
            .unwrap();
            assert!(
                resolve(file)
                    .unwrap_err()
                    .contains("owned by a server-mode Worker"),
                "F3 {role}/{mode}"
            );
        }
        assert!(
            toml::from_str::<FileConfig>(&format!("{source}\nnamespace_alias = \"other\""))
                .is_err(),
            "F4"
        );
    }

    #[test]
    fn kubernetes_trust_policy_is_role_feature_and_credential_exclusive() {
        // K1 Coordinator+Server or AllInOne+Local + no signer + compiled
        // adapter => accepted. K2 every other role/mode/signing-trust
        // combination => rejected before client discovery; K3 missing build
        // feature also fails closed. Cross the complete finite space.
        let policy = KubernetesWorkerTransport {
            namespace: "workers".into(),
            service_account: "executor".into(),
            audience: "awaken-worker".into(),
            provider_id: "cluster-a".into(),
        };
        for role in [
            Role::Coordinator,
            Role::Worker,
            Role::Control,
            Role::AllInOne,
        ] {
            for mode in [
                super::super::OperatingMode::Local,
                super::super::OperatingMode::Server,
            ] {
                for signed in [false, true] {
                    assert_eq!(
                        policy.validate(role, mode, signed).is_ok(),
                        cfg!(feature = "container-k8s")
                            && !signed
                            && ((role == Role::Coordinator
                                && mode == super::super::OperatingMode::Server)
                                || (role == Role::AllInOne
                                    && mode == super::super::OperatingMode::Local)),
                        "K1-K3 {role:?} {mode:?} {signed}"
                    );
                }
            }
        }
        for (field, value) in [
            ("namespace", "../workers"),
            ("service_account", ""),
            ("audience", " "),
            ("provider_id", " "),
        ] {
            let mut bad = policy.clone();
            match field {
                "namespace" => bad.namespace = value.into(),
                "service_account" => bad.service_account = value.into(),
                "audience" => bad.audience = value.into(),
                _ => bad.provider_id = value.into(),
            }
            assert!(
                bad.validate(
                    Role::Coordinator,
                    super::super::OperatingMode::Server,
                    false
                )
                .is_err(),
                "K4 {field}"
            );
        }
    }

    #[test]
    fn credential_files_follow_process_authority() {
        // Cause/effect decision table: R1 Worker receives only its request signer;
        // R2 Coordinator receives only the trust directory; R3 AllInOne owns the
        // paired trust and request views for its one embedded Worker; R4 Control
        // or Worker receiving Coordinator trust -> reject; R5 Coordinator/Control
        // receiving a request signer -> reject; R6 either half of an AllInOne pair
        // -> reject before startup. The pair reuses one transport contract without
        // granting standalone roles another process's authority.
        assert!(
            validate_credential_file_ownership(Role::Worker, true, false).is_ok(),
            "R1"
        );
        assert!(
            validate_credential_file_ownership(Role::Coordinator, false, true).is_ok(),
            "R2"
        );
        assert!(
            validate_credential_file_ownership(Role::AllInOne, true, true).is_ok(),
            "R3"
        );
        assert!(
            validate_credential_file_ownership(Role::Control, false, true).is_err(),
            "R4"
        );
        assert!(
            validate_credential_file_ownership(Role::Worker, false, true).is_err(),
            "R4"
        );
        assert!(
            validate_credential_file_ownership(Role::Coordinator, true, false).is_err(),
            "R5"
        );
        assert!(
            validate_credential_file_ownership(Role::Control, true, false).is_err(),
            "R5"
        );
        assert!(
            validate_credential_file_ownership(Role::AllInOne, true, false).is_err(),
            "R6 request only"
        );
        assert!(
            validate_credential_file_ownership(Role::AllInOne, false, true).is_err(),
            "R6 trust only"
        );
    }
}
