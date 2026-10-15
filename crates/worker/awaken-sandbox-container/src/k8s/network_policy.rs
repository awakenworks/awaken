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

use k8s_openapi::api::networking::v1::NetworkPolicy;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector;
use kube::Api;
use kube::api::ListParams;

use super::error::backend;
use crate::RuntimeError;

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

fn selector_matches(selector: Option<&LabelSelector>, labels: &BTreeMap<String, String>) -> bool {
    // An omitted selector cannot prove exclusion from an additive policy.
    // Treat it conservatively as matching, just like an empty selector.
    selector.is_none_or(|selector| {
        selector
            .match_labels
            .iter()
            .flatten()
            .all(|(key, value)| labels.get(key) == Some(value))
            && selector
                .match_expressions
                .iter()
                .flatten()
                .all(|requirement| {
                    let value = labels.get(&requirement.key);
                    let values = requirement.values.as_deref().unwrap_or_default();
                    match requirement.operator.as_str() {
                        "In" => value.is_some_and(|value| values.contains(value)),
                        "NotIn" => value.is_some_and(|value| !values.contains(value)),
                        "Exists" => value.is_some(),
                        "DoesNotExist" => value.is_none(),
                        _ => false,
                    }
                })
    })
}

fn controls(policy: &NetworkPolicy, policy_type: &str) -> bool {
    policy.spec.as_ref().is_some_and(|spec| {
        spec.policy_types
            .iter()
            .flatten()
            .any(|candidate| candidate == policy_type)
            || (policy_type == "Ingress" && spec.ingress.is_some())
            || (policy_type == "Egress" && spec.egress.is_some())
    })
}

fn allows_egress_to(policy: &NetworkPolicy, labels: &BTreeMap<String, String>) -> bool {
    policy.spec.as_ref().is_some_and(|spec| {
        selector_matches(spec.pod_selector.as_ref(), labels)
            && spec.egress.as_ref().is_some_and(|rules| !rules.is_empty())
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

fn has_exact_namespace(selector: Option<&LabelSelector>) -> bool {
    selector.is_some_and(|selector| {
        selector.match_labels.as_ref().is_some_and(|labels| {
            labels.len() == 1
                && labels
                    .get("kubernetes.io/metadata.name")
                    .is_some_and(|name| !name.trim().is_empty())
                && has_exact_labels(Some(selector), labels)
        })
    })
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
                ) && controls(policy, "Ingress")
                    && controls(policy, "Egress")
                    && spec.ingress.as_ref().is_none_or(Vec::is_empty)
                    && spec.egress.as_ref().is_none_or(Vec::is_empty)
            })
    });
    let open_allow = policies.iter().find(|policy| {
        policy.metadata.name.as_deref() == Some(SANDBOX_OPEN_POLICY)
            && policy.spec.as_ref().is_some_and(|spec| {
                has_exact_labels(spec.pod_selector.as_ref(), &open)
                    && controls(policy, "Egress")
                    && spec.egress.as_ref().is_some_and(|rules| {
                        rules.len() == 1 && rules[0].to.is_none() && rules[0].ports.is_none()
                    })
            })
    });
    deny.is_some()
        && open_allow.is_some()
        && policies
            .iter()
            .all(|policy| !allows_egress_to(policy, &restricted))
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
                    && controls(policy, "Egress")
                    && spec.egress.as_ref().is_some_and(|rules| {
                        rules.len() == 1
                            && rules[0].to.as_ref().is_some_and(|peers| {
                                peers.len() == 1
                                    && peers[0].ip_block.is_none()
                                    && has_exact_namespace(peers[0].namespace_selector.as_ref())
                                    && has_exact_labels(
                                            peers[0].pod_selector.as_ref(),
                                            &BTreeMap::from([(
                                                "app.kubernetes.io/component".to_owned(),
                                                "egress-gateway".to_owned(),
                                            )]),
                                        )
                            })
                            && rules[0].ports.as_ref().is_some_and(|ports| {
                                ports.len() == 1 + usize::from(expected_repository_port.is_some())
                                    && ports.iter().all(|port| {
                                        port.protocol.as_deref() == Some("TCP")
                                            && matches!(port.port.as_ref(), Some(k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(value)) if *value > 0)
                                    })
                                    && expected_repository_port.is_none_or(|expected| {
                                        ports[1].port.as_ref() == Some(&k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(i32::from(expected)))
                                            && ports[0].port != ports[1].port
                                    })
                            })
                    })
            })
    });
    exact.is_some()
        && policies.iter().all(|policy| {
            !allows_egress_to(policy, &allowlist)
                || policy.metadata.name.as_deref() == Some(SANDBOX_ALLOWLIST_POLICY)
        })
}

#[cfg(test)]
mod tests {
    use k8s_openapi::api::networking::v1::{
        NetworkPolicyEgressRule, NetworkPolicyPort, NetworkPolicySpec,
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
        // Independent wire fixture: literal selectors and peers remain the
        // oracle for the runtime labels; do not reuse its label projection.
        serde_json::from_value(serde_json::json!([
            {"metadata":{"name":SANDBOX_DENY_POLICY},"spec":{
                "podSelector":{"matchLabels":{"app":"awaken-sandbox"}},
                "policyTypes":["Ingress","Egress"],"egress":[]}},
            {"metadata":{"name":SANDBOX_OPEN_POLICY},"spec":{
                "podSelector":{"matchLabels":{"app":"awaken-sandbox","awaken-egress":"open"}},
                "policyTypes":["Egress"],"egress":[{}]}},
            {"metadata":{"name":SANDBOX_ALLOWLIST_POLICY},"spec":{
                "podSelector":{"matchLabels":{"app":"awaken-sandbox","awaken-egress":"allowlist"}},
                "policyTypes":["Egress"],"egress":[{
                    "to":[{"namespaceSelector":{"matchLabels":{"kubernetes.io/metadata.name":"awaken-cloud"}},
                        "podSelector":{"matchLabels":{"app.kubernetes.io/component":"egress-gateway"}}}],
                    "ports":[{"protocol":"TCP","port":8081}]}]}}
        ])).unwrap()
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
}
