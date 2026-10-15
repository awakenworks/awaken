use super::*;

// Exercise the shared normalization used by the complete Pod projection.
// This helper adds no alternative defaulting or admission rules.
fn verify_container_projection(expected: &Container, observed: &Container) -> bool {
    let mut expected = expected.clone();
    let mut observed = observed.clone();
    api_defaults::normalize_container(&mut expected);
    api_defaults::normalize_container(&mut observed);
    expected == observed
}

#[tokio::test]
async fn retained_api_defaults_are_symmetric_but_custom_scheduling_stays_exact() {
    // Four rows cross absent/defaulted expected and observed representations.
    // A retained Create response is an expected projection too. Exact API
    // defaults are equivalent in both directions; changing priority or one
    // default-toleration coordinate on either side must remain a mismatch.
    let runtime = K8sRuntime::for_test("127.0.0.1:9000".parse().unwrap());
    let plan = super::super::ephemeral_plan();
    let mut base = runtime.pod("retained-api-defaults", &plan);
    realization::stamp_pod_realization(&mut base).unwrap();
    // Scheduler assignment is observed only when absent from the declaration;
    // a retained or explicitly declared assignment cannot disappear or move.
    for (declared, actual, accepts) in [
        (None, None, true),
        (None, Some("node-a"), true),
        (Some("node-a"), Some("node-a"), true),
        (Some("node-a"), None, false),
        (Some("node-a"), Some("node-b"), false),
    ] {
        let mut expected = base.clone();
        let mut observed = base.clone();
        expected.spec.as_mut().unwrap().node_name = declared.map(str::to_owned);
        observed.spec.as_mut().unwrap().node_name = actual.map(str::to_owned);
        assert_eq!(
            verify_pod_projection(&expected, &observed, None).is_ok(),
            accepts,
            "node row"
        );
    }
    let defaulted = |mut pod: Pod| {
        let spec = pod.spec.as_mut().unwrap();
        spec.priority = Some(0);
        spec.host_users = Some(true);
        spec.host_network = Some(false);
        spec.host_pid = Some(false);
        spec.host_ipc = Some(false);
        spec.share_process_namespace = Some(false);
        spec.set_hostname_as_fqdn = Some(false);
        spec.tolerations = Some(
            [
                "node.kubernetes.io/not-ready",
                "node.kubernetes.io/unreachable",
            ]
            .into_iter()
            .map(|key| k8s_openapi::api::core::v1::Toleration {
                key: Some(key.into()),
                operator: Some("Exists".into()),
                effect: Some("NoExecute".into()),
                toleration_seconds: Some(300),
                ..Default::default()
            })
            .collect(),
        );
        pod
    };
    for expected_defaulted in [false, true] {
        for observed_defaulted in [false, true] {
            let expected = if expected_defaulted {
                defaulted(base.clone())
            } else {
                base.clone()
            };
            let observed = if observed_defaulted {
                defaulted(base.clone())
            } else {
                base.clone()
            };
            assert!(
                verify_pod_projection(&expected, &observed, None).is_ok(),
                "default rows"
            );
        }
    }
    for changed_expected in [false, true] {
        for cause in [
            "priority",
            "key",
            "operator",
            "effect",
            "value",
            "seconds-before",
            "seconds-after",
            "seconds-absent",
        ] {
            let mut expected = defaulted(base.clone());
            let mut observed = defaulted(base.clone());
            let changed = if changed_expected {
                &mut expected
            } else {
                &mut observed
            };
            let spec = changed.spec.as_mut().unwrap();
            let toleration = &mut spec.tolerations.as_mut().unwrap()[0];
            match cause {
                "priority" => spec.priority = Some(1),
                "key" => toleration.key = Some("custom-node".into()),
                "operator" => toleration.operator = Some("Equal".into()),
                "effect" => toleration.effect = Some("NoSchedule".into()),
                "value" => toleration.value = Some("custom-value".into()),
                "seconds-before" => toleration.toleration_seconds = Some(299),
                "seconds-after" => toleration.toleration_seconds = Some(301),
                "seconds-absent" => toleration.toleration_seconds = None,
                _ => unreachable!(),
            }
            assert!(
                verify_pod_projection(&expected, &observed, None).is_err(),
                "{cause}, expected={changed_expected}"
            );
        }
    }
}

#[tokio::test]
async fn api_defaults_never_erase_explicit_security_drift() {
    // Each Boolean default has an independent negative control on either side.
    // Equal API forms pass in the preceding table; flipping one policy value
    // must fail even when the original realization annotations are copied.
    let runtime = K8sRuntime::for_test("127.0.0.1:9000".parse().unwrap());
    let base = runtime.pod("security-defaults", &super::super::ephemeral_plan());
    for (field, default) in [
        ("hostUsers", true),
        ("setHostnameAsFQDN", false),
        ("shareProcessNamespace", false),
        ("hostNetwork", false),
        ("hostPID", false),
        ("hostIPC", false),
        ("enableServiceLinks", true),
    ] {
        // The production Pod deliberately disables service links. Give this
        // row its declared default explicitly before flipping it, so its
        // negative control cannot accidentally repeat an existing false value.
        let mut row = base.clone();
        let mut spec = serde_json::to_value(row.spec.as_ref().unwrap()).unwrap();
        spec[field] = serde_json::Value::Bool(default);
        row.spec = Some(serde_json::from_value(spec).unwrap());
        realization::stamp_pod_realization(&mut row).unwrap();
        assert!(verify_pod_projection(&row, &row, None).is_ok(), "{field}");
        for changed_expected in [false, true] {
            let mut expected = row.clone();
            let mut observed = row.clone();
            let changed = if changed_expected {
                &mut expected
            } else {
                &mut observed
            };
            let mut spec = serde_json::to_value(changed.spec.as_ref().unwrap()).unwrap();
            spec[field] = serde_json::Value::Bool(!default);
            changed.spec = Some(serde_json::from_value(spec).unwrap());
            assert!(
                verify_pod_projection(&expected, &observed, None).is_err(),
                "{field}, expected={changed_expected}"
            );
        }
    }
}

#[tokio::test]
async fn projection_diagnostics_name_fields_without_exposing_execution_values() {
    let runtime = K8sRuntime::for_test("127.0.0.1:9000".parse().unwrap());
    let mut expected = runtime.pod("private-projection-name", &super::super::ephemeral_plan());
    expected.spec.as_mut().unwrap().containers[0].args =
        Some(vec!["private-expected-input".into()]);
    realization::stamp_pod_realization(&mut expected).unwrap();
    let mut observed = expected.clone();
    observed.metadata.name = Some("private-observed-name".into());
    observed.spec.as_mut().unwrap().containers[0].args =
        Some(vec!["private-observed-input".into()]);
    let message = verify_pod_projection(&expected, &observed, None)
        .unwrap_err()
        .to_string();
    assert!(message.contains("containers"));
    for private in [
        "private-projection-name",
        "private-observed-name",
        "private-expected-input",
        "private-observed-input",
    ] {
        assert!(!message.contains(private), "diagnostics leaked {private}");
    }
}

#[tokio::test]
async fn historical_directory_init_keeps_exact_projection_and_digest() {
    // Historical projection decision table: H1 new Pod => no directory init;
    // H2 exact old mkdir/image/mount/security + original digest => adopt without
    // recreating its Pod/PVC; H3 any field or digest drift => reject. H4 a new
    // Pod with copied old provenance is not an old Pod. No security field is
    // ignored and no historical creation path is reintroduced.
    let runtime = K8sRuntime::for_test("127.0.0.1:9000".parse().unwrap()).with_continuation_volume(
        crate::K8sContinuationVolume {
            storage_class_name: None,
            size: "1Gi".into(),
        },
    );
    let plan = crate::k8s::tests::plan_with_memory(Vec::new());
    let mut current = runtime.pod("historical-init", &plan);
    realization::stamp_pod_realization(&mut current).unwrap();
    assert!(
        current.spec.as_ref().unwrap().init_containers.is_none(),
        "H1"
    );
    let mut historical = current.clone();
    let agent = &historical.spec.as_ref().unwrap().containers[0];
    let initializer = Container {
        name: "continuation-init".into(),
        image: agent.image.clone(),
        command: Some(vec![
            "/bin/sh".into(),
            "-c".into(),
            "mkdir -p /state/root-0 /state/root-1 /state/root-2".into(),
        ]),
        volume_mounts: Some(vec![VolumeMount {
            name: "continuation-state".into(),
            mount_path: "/state".into(),
            ..Default::default()
        }]),
        security_context: agent.security_context.clone(),
        ..Default::default()
    };
    historical.spec.as_mut().unwrap().init_containers = Some(vec![initializer]);
    realization::stamp_pod_realization(&mut historical).unwrap();
    assert!(
        verify_pod_projection(&current, &historical, None).is_ok(),
        "H2"
    );
    let once = historical.clone();
    realization::stamp_pod_realization(&mut historical).unwrap();
    assert_eq!(once, historical, "H2 desired stamping is idempotent");
    for cause in [
        "command",
        "image",
        "mount",
        "security",
        "extra",
        "digest",
        "missing_init",
    ] {
        let mut changed = historical.clone();
        let spec = changed.spec.as_mut().unwrap();
        match cause {
            "command" => spec.init_containers.as_mut().unwrap()[0]
                .command
                .as_mut()
                .unwrap()[2]
                .push_str("; touch /state/foreign"),
            "image" => spec.init_containers.as_mut().unwrap()[0].image = Some("foreign:1".into()),
            "mount" => {
                spec.init_containers.as_mut().unwrap()[0]
                    .volume_mounts
                    .as_mut()
                    .unwrap()[0]
                    .mount_path = "/foreign".into()
            }
            "security" => {
                spec.init_containers.as_mut().unwrap()[0]
                    .security_context
                    .as_mut()
                    .unwrap()
                    .privileged = Some(true)
            }
            "extra" => spec.init_containers.as_mut().unwrap().push(Container {
                name: "foreign".into(),
                ..Default::default()
            }),
            "digest" => changed.metadata.annotations = current.metadata.annotations.clone(),
            "missing_init" => spec.init_containers = None,
            _ => unreachable!(),
        }
        assert!(
            verify_pod_projection(&current, &changed, None).is_err(),
            "H3/H4 {cause}"
        );
    }
}

#[test]
fn claim_projection_preserves_policy_across_api_defaulting() {
    // Cause/effect table CP: C1 class is delegated or explicitly pinned;
    // C2 API supplies no class, a named class, or explicit classlessness;
    // C3 binding supplies volumeName; C4 another requested field or digest
    // drifts. CP1 delegated + absent/named -> pass; CP2 pinned + same -> pass;
    // CP3 pinned + absent/other, or delegated + explicit empty -> reject;
    // CP4 C3 alone -> pass (binding output); CP5 any C4 -> reject even with
    // copied provenance. No new cluster-default lookup or policy store.
    for configured in [None, Some("pinned")] {
        let mut expected = continuation::build_claim(
            "restore-proof",
            &crate::K8sContinuationVolume {
                storage_class_name: configured.map(str::to_owned),
                size: "1Gi".into(),
            },
        )
        .unwrap();
        realization::stamp_realization(&mut expected).unwrap();
        // CP5 delegated defaulting rejects empty and whitespace-only names.
        for actual in [
            None,
            Some("pinned"),
            Some("default-class"),
            Some(""),
            Some("  "),
        ] {
            let mut observed = expected.clone();
            let spec = observed.spec.as_mut().unwrap();
            spec.storage_class_name = actual.map(str::to_owned);
            spec.volume_name = Some("api-bound-pv".into());
            let accepts = match configured {
                None => !matches!(actual, Some("" | "  ")),
                Some(pinned) => actual == Some(pinned),
            };
            assert_eq!(
                verify_claim_projection(&expected, &observed, None).is_ok(),
                accepts,
                "CP1-4 configured={configured:?} observed={actual:?}"
            );
        }
        for cause in [
            "name",
            "access",
            "size",
            "mode",
            "selector",
            "source",
            "digest",
            "missing_spec",
        ] {
            let mut changed = expected.clone();
            let spec = changed.spec.as_mut().unwrap();
            match cause {
                "name" => changed.metadata.name = Some("foreign".into()),
                "access" => spec.access_modes = Some(vec!["ReadWriteMany".into()]),
                "size" => {
                    spec.resources
                        .as_mut()
                        .unwrap()
                        .requests
                        .as_mut()
                        .unwrap()
                        .get_mut("storage")
                        .unwrap()
                        .0 = "2Gi".into()
                }
                "mode" => spec.volume_mode = Some("Block".into()),
                "selector" => spec.selector = Some(Default::default()),
                "source" => spec.data_source = Some(Default::default()),
                "digest" => changed.metadata.annotations = None,
                "missing_spec" => changed.spec = None,
                _ => unreachable!(),
            }
            assert!(
                verify_claim_projection(&expected, &changed, None).is_err(),
                "CP5 {cause}"
            );
        }
    }
}

#[test]
fn restore_reuses_the_complete_sandbox_namespace_boundary() {
    // RP1 unchanged/explicit false namespace flags + empty ephemeral list
    // -> same security boundary; RP2 each host/shared namespace or one
    // ephemeral container -> reject, even when the immutable digest matches.
    // This exercises the restore caller of the shared Pod predicate; the
    // predicate's own truth table remains in pod_security tests.
    let mut expected = Pod {
        metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
            name: Some("restore-proof".into()),
            labels: Some(Default::default()),
            ..Default::default()
        },
        spec: Some(Default::default()),
        ..Default::default()
    };
    realization::stamp_realization(&mut expected).unwrap();
    let mut compatible = expected.clone();
    let spec = compatible.spec.as_mut().unwrap();
    spec.host_network = Some(false);
    spec.host_pid = Some(false);
    spec.host_ipc = Some(false);
    spec.share_process_namespace = Some(false);
    spec.ephemeral_containers = Some(vec![]);
    assert!(
        verify_pod_projection(&expected, &compatible, None).is_ok(),
        "RP1"
    );
    let mutations: [crate::k8s::PodSpecMutation; 5] = [
        ("host network", |spec| spec.host_network = Some(true)),
        ("host PID", |spec| spec.host_pid = Some(true)),
        ("host IPC", |spec| spec.host_ipc = Some(true)),
        ("shared processes", |spec| {
            spec.share_process_namespace = Some(true)
        }),
        ("ephemeral container", |spec| {
            spec.ephemeral_containers = Some(vec![Default::default()])
        }),
    ];
    for (cause, mutate) in mutations {
        let mut changed = compatible.clone();
        mutate(changed.spec.as_mut().unwrap());
        assert!(
            verify_pod_projection(&expected, &changed, None).is_err(),
            "RP2 {cause}"
        );
    }
}
#[test]
fn container_resources_allow_only_empty_api_defaulting() {
    // CR1 omitted/empty resources are equivalent; CR2 declared requests
    // remain exact. Cross both directions: API defaulting must neither
    // invent a required limit nor conceal removal of a frozen reservation.
    let empty = k8s_openapi::api::core::v1::ResourceRequirements::default();
    let requested = k8s_openapi::api::core::v1::ResourceRequirements {
        requests: Some(std::collections::BTreeMap::from([(
            "cpu".into(),
            k8s_openapi::apimachinery::pkg::api::resource::Quantity("10m".into()),
        )])),
        ..Default::default()
    };
    for (before, after, accepts) in [
        (None, None, true),
        (None, Some(empty.clone()), true),
        (Some(empty.clone()), None, true),
        (Some(requested.clone()), None, false),
        (None, Some(requested.clone()), false),
        (Some(requested.clone()), Some(empty), false),
        (Some(requested.clone()), Some(requested), true),
    ] {
        let expected = Container {
            resources: before,
            ..Default::default()
        };
        let observed = Container {
            resources: after,
            ..Default::default()
        };
        assert_eq!(
            verify_container_projection(&expected, &observed),
            accepts,
            "CR1/CR2 expected={:?} observed={:?}",
            expected.resources,
            observed.resources
        );
    }
}

#[test]
fn restored_pod_accepts_api_forms_without_accepting_extra_inputs() {
    use k8s_openapi::api::core::v1::{
        EnvFromSource, PersistentVolumeClaimVolumeSource, Volume, VolumeMount,
    };
    // AP graph: C1 empty env/resources and false mount/PVC flags are omitted
    // by the apiserver; C2 image default pull policy is populated; C3 mutable
    // inputs/security/scheduling spec change while provenance is copied.
    // AP1 C1+C2 => accept same Pod; AP2 each C3 => reject. Both agent and init
    // containers traverse this one verifier; init ordering must stay exact.
    let mut expected = Pod {
        metadata: ObjectMeta {
            name: Some("api-forms".into()),
            labels: Some(Default::default()),
            ..Default::default()
        },
        spec: Some(PodSpec {
            containers: vec![Container {
                name: "agent".into(),
                image: Some("registry/agent:1".into()),
                env: Some(vec![]),
                volume_mounts: Some(vec![VolumeMount {
                    name: "state".into(),
                    mount_path: "/workspace".into(),
                    read_only: Some(false),
                    ..Default::default()
                }]),
                ..Default::default()
            }],
            volumes: Some(vec![Volume {
                name: "state".into(),
                persistent_volume_claim: Some(PersistentVolumeClaimVolumeSource {
                    claim_name: "claim-a".into(),
                    read_only: Some(false),
                }),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        ..Default::default()
    };
    expected.spec.as_mut().unwrap().init_containers = Some(
        ["prepare", "seed"]
            .into_iter()
            .map(|name| Container {
                name: name.into(),
                image: Some("registry/agent:1".into()),
                ..Default::default()
            })
            .collect(),
    );
    realization::stamp_realization(&mut expected).unwrap();
    let mut observed = expected.clone();
    let spec = observed.spec.as_mut().unwrap();
    let container = &mut spec.containers[0];
    container.env = None;
    container.resources = Some(Default::default());
    container.volume_mounts.as_mut().unwrap()[0].read_only = None;
    container.image_pull_policy = Some("IfNotPresent".into());
    container.termination_message_path = Some("/dev/termination-log".into());
    container.termination_message_policy = Some("File".into());
    spec.volumes.as_mut().unwrap()[0]
        .persistent_volume_claim
        .as_mut()
        .unwrap()
        .read_only = None;
    assert!(
        verify_pod_projection(&expected, &observed, None).is_ok(),
        "AP1"
    );
    for cause in [
        "env_from",
        "working_dir",
        "read_only",
        "claim",
        "image",
        "pull",
        "dns",
        "init",
        "init_order",
    ] {
        let mut changed = observed.clone();
        let spec = changed.spec.as_mut().unwrap();
        let container = &mut spec.containers[0];
        match cause {
            "env_from" => {
                container.env_from = Some(vec![EnvFromSource {
                    prefix: Some("foreign".into()),
                    ..Default::default()
                }])
            }
            "working_dir" => container.working_dir = Some("/elsewhere".into()),
            "read_only" => container.volume_mounts.as_mut().unwrap()[0].read_only = Some(true),
            "claim" => {
                spec.volumes.as_mut().unwrap()[0]
                    .persistent_volume_claim
                    .as_mut()
                    .unwrap()
                    .claim_name = "claim-b".into()
            }
            "image" => container.image = Some("foreign/image:1".into()),
            "pull" => container.image_pull_policy = Some("Always".into()),
            "dns" => spec.dns_policy = Some("Default".into()),
            "init" => spec.init_containers = Some(vec![container.clone()]),
            "init_order" => spec.init_containers.as_mut().unwrap().reverse(),
            _ => unreachable!(),
        }
        assert!(
            verify_pod_projection(&expected, &changed, None).is_err(),
            "AP2 {cause}"
        );
    }
}

#[test]
fn image_policy_defaulting_respects_tag_and_digest() {
    // IP1 no tag/latest => Always; IP2 named tag/digest => IfNotPresent.
    // Registry ports do not count as tags. IP3 explicit opposite policy remains
    // unequal: defaulting is not permission to drop a configured pull policy.
    for (image, policy) in [
        ("registry:5000/agent", "Always"),
        ("agent:latest", "Always"),
        ("registry/agent:1", "IfNotPresent"),
        ("agent@sha256:fixture", "IfNotPresent"),
    ] {
        let expected = Container {
            image: Some(image.into()),
            ..Default::default()
        };
        let mut observed = expected.clone();
        observed.image_pull_policy = Some(policy.into());
        assert!(
            verify_container_projection(&expected, &observed),
            "IP1/2 {image}"
        );
        observed.image_pull_policy = Some(
            if policy == "Always" {
                "IfNotPresent"
            } else {
                "Always"
            }
            .into(),
        );
        assert!(
            !verify_container_projection(&expected, &observed),
            "IP3 {image}"
        );
    }
}

#[test]
fn deleting_projection_requires_authorized_disposal_and_exact_identity() {
    // DP graph: C1 live/deleting object; C2 ordinary recovery/authorized
    // disposal; C3 exact/copied-digest-with-spec-drift/missing-digest.
    // DP1 exact + live => accept either operation; DP2 exact + deleting =>
    // accept disposal only; DP3 any drift => reject either operation.
    // Both Pod and PVC use this table; authorization permits inspection,
    // never physical deletion or adoption by itself.
    let fence =
        pc::SandboxEffectFence::new("projection-cleanup", "owner", "runtime", 1, u64::MAX).unwrap();
    let authorization = crate::tests::disposal_authorization(&fence);
    let mut pod = Pod {
        metadata: ObjectMeta {
            name: Some("disposal-projection".into()),
            labels: Some(Default::default()),
            ..Default::default()
        },
        spec: Some(Default::default()),
        ..Default::default()
    };
    let mut claim = continuation::build_claim(
        "disposal-projection",
        &crate::K8sContinuationVolume {
            storage_class_name: None,
            size: "1Gi".into(),
        },
    )
    .unwrap();
    realization::stamp_realization(&mut pod).unwrap();
    realization::stamp_realization(&mut claim).unwrap();
    for deleting in [false, true] {
        for disposal in [None, Some(&authorization)] {
            for drift in ["none", "spec", "digest"] {
                let mut observed_pod = pod.clone();
                let mut observed_claim = claim.clone();
                if deleting {
                    let timestamp =
                        serde_json::from_value(serde_json::json!("2026-09-06T00:00:00Z")).unwrap();
                    observed_pod.metadata.deletion_timestamp = Some(timestamp);
                    observed_claim.metadata.deletion_timestamp =
                        observed_pod.metadata.deletion_timestamp.clone();
                }
                match drift {
                    "spec" => {
                        observed_pod.spec.as_mut().unwrap().host_network = Some(true);
                        observed_claim.spec.as_mut().unwrap().volume_mode = Some("Block".into());
                    }
                    "digest" => {
                        observed_pod.metadata.annotations = None;
                        observed_claim.metadata.annotations = None;
                    }
                    _ => {}
                }
                let accepts = drift == "none" && (!deleting || disposal.is_some());
                assert_eq!(
                    verify_pod_projection(&pod, &observed_pod, disposal).is_ok(),
                    accepts,
                    "DP Pod deleting={deleting} disposal={} drift={drift}",
                    disposal.is_some()
                );
                assert_eq!(
                    verify_claim_projection(&claim, &observed_claim, disposal).is_ok(),
                    accepts,
                    "DP PVC deleting={deleting} disposal={} drift={drift}",
                    disposal.is_some()
                );
            }
        }
    }
}

#[test]
fn every_probe_role_preserves_behavior_across_zero_delay_omission() {
    // PR graph: C1 readiness/liveness/startup; C2 omitted or zero delay;
    // C3 positive delay; C4 changed command or removed probe. PR1 C2 alone
    // => equivalent; PR2 C3/C4 => reject. Cross every role because the same
    // container projection must protect all three independent probe contracts.
    for role in ["readinessProbe", "livenessProbe", "startupProbe"] {
        let expected: Container = serde_json::from_value(serde_json::json!({
            "name": "agent", (role): {
                "exec": {"command": ["/trusted/check"]}, "initialDelaySeconds": 0
            }
        }))
        .unwrap();
        for change in ["omitted", "positive", "command", "removed"] {
            let mut wire = serde_json::to_value(&expected).unwrap();
            let probe = wire[role].as_object_mut().unwrap();
            probe.remove("initialDelaySeconds");
            match change {
                "positive" => {
                    probe.insert("initialDelaySeconds".into(), serde_json::json!(1));
                }
                "command" => {
                    probe.insert("exec".into(), serde_json::json!({"command": ["/bin/true"]}));
                }
                "removed" => {
                    wire.as_object_mut().unwrap().remove(role);
                }
                _ => {}
            }
            let observed = serde_json::from_value(wire).unwrap();
            assert_eq!(
                verify_container_projection(&expected, &observed),
                change == "omitted",
                "PR {role}/{change}"
            );
            assert_eq!(
                verify_container_projection(&observed, &expected),
                change == "omitted",
                "PR reverse {role}/{change}"
            );
        }
    }
}
