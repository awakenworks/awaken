// @formal surface obligation=sandbox.admission_never_weakens_isolation
// @formal surface obligation=sandbox.fail_closed_policy_never_downgrades
// @formal surface obligation=external.container_and_kernel_isolation
// @formal reason: Live Kubernetes NetworkPolicy attestation fails closed unless the exact deny/open policy
// @formal reason: graph exists without additive restricted-egress widening.
//! Live Kubernetes NetworkPolicy attestation for the canonical sandbox labels.
//!
//! NetworkPolicy resources remain the enforcement authority. This module only
//! proves that the live namespace contains the exact deny/open contract and
//! that no additive egress policy widens a restricted sandbox.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};

use k8s_openapi::api::networking::v1::{NetworkPolicy, NetworkPolicyEgressRule, NetworkPolicySpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector;
use kube::Api;
use kube::api::ListParams;

use super::error::backend;
use awaken_container_contract::RuntimeError;

const SANDBOX_DENY_POLICY: &str = "awaken-sandbox-default-deny";
const SANDBOX_OPEN_POLICY: &str = "awaken-sandbox-open-egress";
const SANDBOX_ALLOWLIST_POLICY: &str = "awaken-sandbox-allowlist-egress";

/// Mutable live evidence owned by one runtime instance. All refresh and
/// fail-closed state transitions pass through this type.
pub(super) struct Attestation {
    network_none: AtomicBool,
    network_allowlist: AtomicBool,
    expected_repository_port: Option<u16>,
}

impl Attestation {
    pub(super) const fn new(expected_repository_port: Option<u16>) -> Self {
        Self {
            network_none: AtomicBool::new(false),
            network_allowlist: AtomicBool::new(false),
            expected_repository_port,
        }
    }

    pub(super) fn current(&self) -> bool {
        self.network_none.load(Ordering::Acquire)
    }

    pub(super) fn allowlist_current(&self) -> bool {
        self.network_allowlist.load(Ordering::Acquire)
    }

    pub(super) async fn refresh(
        &self,
        client: &kube::Client,
        namespace: &str,
    ) -> Result<(), RuntimeError> {
        let attested = match attest(client, namespace, self.expected_repository_port).await {
            Ok(attested) => attested,
            Err(error) => {
                self.network_none.store(false, Ordering::Release);
                self.network_allowlist.store(false, Ordering::Release);
                return Err(error);
            }
        };
        self.network_none
            .store(attested.network_none, Ordering::Release);
        self.network_allowlist
            .store(attested.network_allowlist, Ordering::Release);
        if attested.network_none {
            Ok(())
        } else {
            Err(backend(format!(
                "namespace {namespace:?} does not satisfy the canonical sandbox NetworkPolicy contract"
            )))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Evidence {
    network_none: bool,
    network_allowlist: bool,
}

pub(super) async fn attest(
    client: &kube::Client,
    namespace: &str,
    expected_repository_port: Option<u16>,
) -> Result<Evidence, RuntimeError> {
    let policies: Api<NetworkPolicy> = Api::namespaced(client.clone(), namespace);
    let policies = policies
        .list(&ListParams::default())
        .await
        .map_err(backend)?;
    Ok(Evidence {
        network_none: attests_contract(&policies.items),
        network_allowlist: attests_allowlist_contract(&policies.items, expected_repository_port),
    })
}

fn selector_may_match(selector: Option<&LabelSelector>, labels: &BTreeMap<String, String>) -> bool {
    // Attestation knows the sandbox's network classification, not every label
    // a realization can carry. Missing selectors/coordinates cannot prove that
    // an additive allow rule excludes the sandbox.
    let Some(selector) = selector else {
        return true;
    };
    if !selector.match_labels.as_ref().is_none_or(|expected| {
        expected
            .iter()
            .all(|(key, value)| labels.get(key).is_none_or(|actual| actual == value))
    }) {
        return false;
    }
    selector
        .match_expressions
        .as_ref()
        .is_none_or(|requirements| {
            requirements.iter().all(|requirement| {
                let value = labels.get(&requirement.key);
                let values = requirement.values.as_deref().unwrap_or_default();
                match requirement.operator.as_str() {
                    "In" if !values.is_empty() => value.is_none_or(|value| values.contains(value)),
                    "NotIn" if !values.is_empty() => {
                        value.is_none_or(|value| !values.contains(value))
                    }
                    "Exists" if values.is_empty() => true,
                    "DoesNotExist" if values.is_empty() => value.is_none(),
                    _ => true,
                }
            })
        })
}

fn controls(spec: &NetworkPolicySpec, policy_type: &str) -> bool {
    spec.policy_types
        .as_ref()
        .is_some_and(|types| types.iter().any(|candidate| candidate == policy_type))
        || (policy_type == "Ingress" && spec.ingress.is_some())
        || (policy_type == "Egress" && spec.egress.is_some())
}

fn excludes_egress(policy: &NetworkPolicy, labels: &BTreeMap<String, String>) -> bool {
    policy.spec.as_ref().is_some_and(|spec| {
        !selector_may_match(spec.pod_selector.as_ref(), labels)
            || spec.egress.as_ref().is_none_or(Vec::is_empty)
    })
}

fn excludes_ingress(policy: &NetworkPolicy, labels: &BTreeMap<String, String>) -> bool {
    policy.spec.as_ref().is_some_and(|spec| {
        !selector_may_match(spec.pod_selector.as_ref(), labels)
            || spec.ingress.as_ref().is_none_or(Vec::is_empty)
    })
}

fn has_exact_labels(selector: Option<&LabelSelector>, expected: &BTreeMap<String, String>) -> bool {
    selector.is_some_and(|selector| {
        selector.match_labels.as_ref() == Some(expected)
            && selector
                .match_expressions
                .as_ref()
                .is_none_or(Vec::is_empty)
    })
}

fn ingress_is_not_widened(policies: &[NetworkPolicy]) -> bool {
    ["restricted", "open", "allowlist"].into_iter().all(|mode| {
        let labels = super::sandbox_network_labels(Some(mode));
        policies
            .iter()
            .all(|policy| excludes_ingress(policy, &labels))
    })
}

fn has_exact_namespace(selector: Option<&LabelSelector>) -> bool {
    let Some(labels) = selector.and_then(|selector| selector.match_labels.as_ref()) else {
        return false;
    };
    labels.len() == 1
        && labels
            .get("kubernetes.io/metadata.name")
            .is_some_and(|name| !name.trim().is_empty())
        && has_exact_labels(selector, labels)
}

fn attests_contract(policies: &[NetworkPolicy]) -> bool {
    let restricted = super::sandbox_network_labels(Some("restricted"));
    let open = super::sandbox_network_labels(Some("open"));

    let deny = policies.iter().find(|policy| {
        policy.metadata.name.as_deref() == Some(SANDBOX_DENY_POLICY)
            && policy.spec.as_ref().is_some_and(|spec| {
                has_exact_labels(
                    spec.pod_selector.as_ref(),
                    &super::sandbox_network_labels(None),
                ) && controls(spec, "Ingress")
                    && controls(spec, "Egress")
                    && spec.ingress.as_ref().is_none_or(Vec::is_empty)
                    && spec.egress.as_ref().is_none_or(Vec::is_empty)
            })
    });
    let open_allow = policies.iter().find(|policy| {
        policy.metadata.name.as_deref() == Some(SANDBOX_OPEN_POLICY)
            && policy.spec.as_ref().is_some_and(|spec| {
                has_exact_labels(spec.pod_selector.as_ref(), &open)
                    && controls(spec, "Egress")
                    && spec.egress.as_ref().is_some_and(|rules| {
                        rules.len() == 1 && rules[0].to.is_none() && rules[0].ports.is_none()
                    })
            })
    });
    let restricted_is_not_widened = policies
        .iter()
        .all(|policy| excludes_egress(policy, &restricted));

    deny.is_some()
        && open_allow.is_some()
        && restricted_is_not_widened
        && ingress_is_not_widened(policies)
}

fn attests_allowlist_contract(
    policies: &[NetworkPolicy],
    expected_repository_port: Option<u16>,
) -> bool {
    let allowlist = super::sandbox_network_labels(Some("allowlist"));
    let exact = policies.iter().find(|policy| {
        policy.metadata.name.as_deref() == Some(SANDBOX_ALLOWLIST_POLICY)
            && policy.spec.as_ref().is_some_and(|spec| {
                has_exact_labels(spec.pod_selector.as_ref(), &allowlist)
                    && controls(spec, "Egress")
                    && matches!(spec.egress.as_deref(), Some([rule]) if exact_gateway_rule(rule, expected_repository_port))
            })
    });
    exact.is_some()
        && attests_contract(policies)
        && policies.iter().all(|policy| {
            policy.metadata.name.as_deref() == Some(SANDBOX_ALLOWLIST_POLICY)
                || excludes_egress(policy, &allowlist)
        })
}

fn exact_gateway_rule(rule: &NetworkPolicyEgressRule, repository_port: Option<u16>) -> bool {
    let (Some([peer]), Some(ports)) = (rule.to.as_deref(), rule.ports.as_deref()) else {
        return false;
    };
    peer.ip_block.is_none()
        && has_exact_namespace(peer.namespace_selector.as_ref())
        && has_exact_labels(peer.pod_selector.as_ref(), &BTreeMap::from([(
            "app.kubernetes.io/component".into(), "egress-gateway".into(),
        )]))
        && ports.len() == 1 + usize::from(repository_port.is_some())
        && ports.iter().all(|port| {
            port.protocol.as_deref() == Some("TCP") && port.end_port.is_none()
                && matches!(port.port.as_ref(), Some(k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(value)) if *value > 0)
        })
        && repository_port.is_none_or(|expected| {
            ports[1].port.as_ref() == Some(&k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(i32::from(expected)))
                && ports[0].port != ports[1].port
        })
}

#[cfg(test)]
mod tests {
    use k8s_openapi::api::networking::v1::{
        NetworkPolicyEgressRule, NetworkPolicyIngressRule, NetworkPolicyPeer, NetworkPolicyPort,
        NetworkPolicySpec,
    };
    use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelectorRequirement, ObjectMeta};
    use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;

    use super::*;

    fn policy(
        name: &str,
        selector: LabelSelector,
        policy_types: &[&str],
        egress: Option<Vec<NetworkPolicyEgressRule>>,
    ) -> NetworkPolicy {
        NetworkPolicy {
            metadata: ObjectMeta {
                name: Some(name.into()),
                ..Default::default()
            },
            spec: Some(NetworkPolicySpec {
                pod_selector: Some(selector),
                policy_types: Some(policy_types.iter().map(|value| (*value).into()).collect()),
                ingress: None,
                egress,
            }),
        }
    }

    fn canonical() -> Vec<NetworkPolicy> {
        vec![
            policy(
                SANDBOX_DENY_POLICY,
                LabelSelector {
                    match_labels: Some(BTreeMap::from([("app".into(), "awaken-sandbox".into())])),
                    ..Default::default()
                },
                &["Ingress", "Egress"],
                Some(Vec::new()),
            ),
            policy(
                SANDBOX_OPEN_POLICY,
                LabelSelector {
                    match_labels: Some(BTreeMap::from([
                        ("app".into(), "awaken-sandbox".into()),
                        ("awaken-egress".into(), "open".into()),
                    ])),
                    ..Default::default()
                },
                &["Egress"],
                Some(vec![NetworkPolicyEgressRule::default()]),
            ),
            policy(
                SANDBOX_ALLOWLIST_POLICY,
                LabelSelector {
                    match_labels: Some(BTreeMap::from([
                        ("app".into(), "awaken-sandbox".into()),
                        ("awaken-egress".into(), "allowlist".into()),
                    ])),
                    ..Default::default()
                },
                &["Egress"],
                Some(vec![NetworkPolicyEgressRule {
                    to: Some(vec![NetworkPolicyPeer {
                        namespace_selector: Some(LabelSelector {
                            match_labels: Some(BTreeMap::from([(
                                "kubernetes.io/metadata.name".into(),
                                "awaken-cloud".into(),
                            )])),
                            ..Default::default()
                        }),
                        pod_selector: Some(LabelSelector {
                            match_labels: Some(BTreeMap::from([(
                                "app.kubernetes.io/component".into(),
                                "egress-gateway".into(),
                            )])),
                            ..Default::default()
                        }),
                        ..Default::default()
                    }]),
                    ports: Some(vec![NetworkPolicyPort {
                        protocol: Some("TCP".into()),
                        port: Some(IntOrString::Int(8081)),
                        end_port: None,
                    }]),
                }]),
            ),
        ]
    }

    #[test]
    fn exact_live_policy_graph_is_required_without_additive_widening() {
        // Cause/effect graph: C1 exact sandbox deny exists; C2 open-only allow
        // exists; C3 another egress policy selects restricted Pods; C4 the
        // allowlist peer has exactly one explicit, non-wildcard namespace.
        // Effect E1 advertise/admit isolation iff C1+C2+!C3+C4. Rules: P1
        // canonical=>E1; P2 missing deny=>reject; P3 namespace-wide allow=>
        // reject; P4 absent namespace selector=>reject; P5 an explicit sandbox
        // exclusion preserves E1.
        let canonical = canonical();
        assert!(attests_contract(&canonical), "P1");
        assert!(attests_allowlist_contract(&canonical, None), "P1 allowlist");
        assert!(!attests_contract(&canonical[1..]), "P2");

        let mut widened = canonical.clone();
        widened.push(policy(
            "dns-egress",
            LabelSelector::default(),
            &["Egress"],
            Some(vec![NetworkPolicyEgressRule::default()]),
        ));
        assert!(!attests_contract(&widened), "P3");
        assert!(!attests_allowlist_contract(&widened, None), "P3 allowlist");

        let mut wildcard_namespace = canonical.clone();
        wildcard_namespace[2]
            .spec
            .as_mut()
            .unwrap()
            .egress
            .as_mut()
            .unwrap()[0]
            .to
            .as_mut()
            .unwrap()[0]
            .namespace_selector = Some(LabelSelector::default());
        assert!(
            !attests_allowlist_contract(&wildcard_namespace, None),
            "a wildcard namespace can select an attacker-controlled gateway"
        );

        let mut missing_namespace = canonical.clone();
        missing_namespace[2]
            .spec
            .as_mut()
            .unwrap()
            .egress
            .as_mut()
            .unwrap()[0]
            .to
            .as_mut()
            .unwrap()[0]
            .namespace_selector = None;
        assert!(
            !attests_allowlist_contract(&missing_namespace, None),
            "P4 an absent namespace selector spans every namespace"
        );

        let mut excluded = canonical;
        excluded.push(policy(
            "dns-egress",
            LabelSelector {
                match_expressions: Some(vec![LabelSelectorRequirement {
                    key: "app".into(),
                    operator: "NotIn".into(),
                    values: Some(vec!["awaken-sandbox".into()]),
                }]),
                ..Default::default()
            },
            &["Egress"],
            Some(vec![NetworkPolicyEgressRule::default()]),
        ));
        assert!(attests_contract(&excluded), "P5");
    }

    #[test]
    fn missing_policy_coordinates_never_establish_isolation() {
        // Every required policy needs both its spec and exact selector. Missing
        // observations cannot become empty selectors or a partial allowlist proof.
        for index in 0..3 {
            let mut policies = canonical();
            policies[index].spec.as_mut().unwrap().pod_selector = None;
            assert!(!attests_contract(&policies), "selector {index}");
            assert!(
                !attests_allowlist_contract(&policies, None),
                "selector {index}"
            );
            policies[index].spec = None;
            assert!(!attests_contract(&policies), "spec {index}");
            assert!(!attests_allowlist_contract(&policies, None), "spec {index}");
        }
        assert!(!attests_allowlist_contract(&canonical()[1..], None));
    }

    #[test]
    fn unknown_selectors_cannot_hide_additive_egress() {
        // Missing/empty selectors, unobserved labels, negative membership and
        // malformed requirements may select a sandbox. Only a proved exclusion
        // may keep the existing isolation evidence (the earlier P5 control).
        let requirement = |key: &str, operator: &str, values: &[&str]| {
            Some(LabelSelector {
                match_expressions: Some(vec![LabelSelectorRequirement {
                    key: key.into(),
                    operator: operator.into(),
                    values: Some(values.iter().map(|value| (*value).into()).collect()),
                }]),
                ..Default::default()
            })
        };
        let selectors = [
            None,
            Some(LabelSelector::default()),
            Some(LabelSelector {
                match_labels: Some(BTreeMap::from([(
                    "awaken-session".into(),
                    "possible-session".into(),
                )])),
                ..Default::default()
            }),
            requirement("unobserved", "In", &["possible"]),
            requirement("unobserved", "NotIn", &["foreign"]),
            requirement("unobserved", "Exists", &[]),
            requirement("unobserved", "DoesNotExist", &[]),
            requirement("app", "Unrecognized", &["awaken-sandbox"]),
            requirement("app", "In", &[]),
            requirement("app", "Exists", &["invalid"]),
        ];
        for selector in selectors {
            let mut extra = policy(
                "additive",
                LabelSelector::default(),
                &["Egress"],
                Some(vec![NetworkPolicyEgressRule::default()]),
            );
            extra.spec.as_mut().unwrap().pod_selector = selector.clone();
            let mut policies = canonical();
            policies.push(extra);
            assert!(!attests_contract(&policies), "{selector:?}");
            assert!(!attests_allowlist_contract(&policies, None), "{selector:?}");
        }
    }

    #[test]
    fn additive_ingress_is_rejected_for_every_sandbox_class() {
        // Each network class keeps the canonical ingress deny. An unrelated
        // app is a concrete exclusion; missing/unknown coordinates are not.
        for mode in ["restricted", "open", "allowlist", "unrelated"] {
            let selector = LabelSelector {
                match_labels: Some(if mode == "unrelated" {
                    BTreeMap::from([("app".into(), "other-application".into())])
                } else {
                    super::super::sandbox_network_labels(Some(mode))
                }),
                ..Default::default()
            };
            let mut extra = policy("additive-ingress", selector, &["Ingress"], None);
            extra.spec.as_mut().unwrap().ingress = Some(vec![NetworkPolicyIngressRule::default()]);
            let mut policies = canonical();
            policies.push(extra);
            assert_eq!(attests_contract(&policies), mode == "unrelated", "{mode}");
            assert_eq!(
                attests_allowlist_contract(&policies, None),
                mode == "unrelated",
                "{mode}"
            );
        }
    }

    #[test]
    fn published_product_ingress_explicitly_excludes_sandbox_labels() {
        // The real product overlay shares the Sandbox namespace. Its backend
        // ingress may use labels outside the attestor's known coordinates, so
        // it must explicitly exclude the immutable Sandbox app label.
        let documents = serde_saphyr::from_multiple::<serde_json::Value>(include_str!(
            "../../../../deploy/k3d/product-backend/resources.yaml"
        ))
        .unwrap();
        let ingress = documents
            .into_iter()
            .filter(|document| document["kind"] == "NetworkPolicy")
            .map(|document| serde_json::from_value::<NetworkPolicy>(document).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(ingress.len(), 1, "the published overlay's ingress contract");
        let mut policies = canonical();
        policies.extend(ingress);
        assert!(attests_contract(&policies));
        assert!(attests_allowlist_contract(&policies, None));
        policies
            .last_mut()
            .unwrap()
            .spec
            .as_mut()
            .unwrap()
            .pod_selector
            .as_mut()
            .unwrap()
            .match_expressions = None;
        assert!(
            !attests_contract(&policies),
            "unobserved labels cannot prove exclusion"
        );
        assert!(!attests_allowlist_contract(&policies, None));
    }

    #[test]
    fn distributed_overlay_role_policies_exclude_sandbox_labels() {
        // Cause/effect: C1 the distributed overlay imports the canonical base
        // into the Workers' Sandbox namespace; C2 a role policy selects by a
        // label the attestor cannot observe (database-role). D1 C1 + every
        // C2 policy also excluding the immutable Sandbox app label => attests.
        // D2 dropping that exclusion => fail closed, so the Worker never
        // becomes Ready rather than trusting an unobservable label.
        let documents = [
            include_str!("../../../../deploy/k3d/bases/sandbox-network-policy/resources.yaml"),
            include_str!("../../../../deploy/k3d/distributed-control/network-policies.yaml"),
        ]
        .into_iter()
        .flat_map(|yaml| serde_saphyr::from_multiple::<serde_json::Value>(yaml).unwrap());
        let mut policies = documents
            .filter(|document| document["kind"] == "NetworkPolicy")
            .map(|document| serde_json::from_value::<NetworkPolicy>(document).unwrap())
            .collect::<Vec<_>>();
        assert!(attests_contract(&policies), "D1");
        let postgres = policies
            .iter_mut()
            .find(|policy| policy.metadata.name.as_deref() == Some("postgres"))
            .unwrap();
        postgres
            .spec
            .as_mut()
            .unwrap()
            .pod_selector
            .as_mut()
            .unwrap()
            .match_expressions
            .as_mut()
            .unwrap()
            .retain(|requirement| requirement.key != "app");
        assert!(!attests_contract(&policies), "D2");
    }

    #[test]
    fn private_repository_port_must_share_the_one_gateway_peer() {
        // Causes: C1=proxy-only vs proxy+private-Git ports; C2=both ports TCP,
        // distinct and on the same exact Gateway peer; C3=third, duplicate,
        // UDP, or separate peer. Effects: E1=one-port standalone admitted;
        // E2=two-port hosted admitted; E3=widened graph rejected.
        // Rules: G1(1,exact)->E1; G2(2,C2)->E2; G3(C3)->E3.
        fn ports(policies: &mut [NetworkPolicy]) -> &mut Vec<NetworkPolicyPort> {
            policies[2].spec.as_mut().unwrap().egress.as_mut().unwrap()[0]
                .ports
                .as_mut()
                .unwrap()
        }
        let mut policies = canonical();
        assert!(attests_allowlist_contract(&policies, None), "G1");
        ports(&mut policies).push(NetworkPolicyPort {
            protocol: Some("TCP".into()),
            port: Some(IntOrString::Int(8444)),
            end_port: None,
        });
        assert!(attests_allowlist_contract(&policies, Some(8444)), "G2");
        assert!(!attests_allowlist_contract(&policies, None), "G3 no trust");
        assert!(
            !attests_allowlist_contract(&policies, Some(8090)),
            "G3 wrong port"
        );
        ports(&mut policies)[1].port = Some(IntOrString::Int(8081));
        assert!(
            !attests_allowlist_contract(&policies, Some(8444)),
            "G3 duplicate"
        );
        ports(&mut policies)[1].port = Some(IntOrString::Int(8444));
        ports(&mut policies)[1].protocol = Some("UDP".into());
        assert!(!attests_allowlist_contract(&policies, Some(8444)), "G3 UDP");
        ports(&mut policies)[1].protocol = Some("TCP".into());
        ports(&mut policies)[1].end_port = Some(9000);
        assert!(
            !attests_allowlist_contract(&policies, Some(8444)),
            "G3 range"
        );
        ports(&mut policies)[1].end_port = None;
        ports(&mut policies).push(NetworkPolicyPort {
            protocol: Some("TCP".into()),
            port: Some(IntOrString::Int(9000)),
            end_port: None,
        });
        assert!(
            !attests_allowlist_contract(&policies, Some(8444)),
            "G3 third port"
        );
    }

    #[test]
    fn an_omitted_selector_cannot_attest_exact_labels_or_hide_additive_egress() {
        // P6 newly optional generated selector + named canonical rule -> no
        // exact-label proof. P7 another egress rule with no selector -> may
        // select all Pods, so both restricted and allowlist evidence fail.
        // This preserves the original attestation owner and never substitutes
        // guessed labels or an empty named canonical contract.
        for index in 0..3 {
            let mut policies = canonical();
            policies[index].spec.as_mut().unwrap().pod_selector = None;
            if index < 2 {
                assert!(!attests_contract(&policies), "P6");
            } else {
                assert!(!attests_allowlist_contract(&policies, None), "P6 allowlist");
            }
        }
        let mut policies = canonical();
        let mut extra = policy(
            "unqualified-egress",
            LabelSelector::default(),
            &["Egress"],
            Some(vec![NetworkPolicyEgressRule::default()]),
        );
        extra.spec.as_mut().unwrap().pod_selector = None;
        policies.push(extra);
        assert!(!attests_contract(&policies), "P7");
        assert!(!attests_allowlist_contract(&policies, None), "P7 allowlist");
    }
}
